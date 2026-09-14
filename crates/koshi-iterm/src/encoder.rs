//! Bounded iTerm2 OSC 1337 image output.

use std::io::{self, Write};

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use image::codecs::png::{CompressionType, FilterType, PngEncoder};
use image::{ExtendedColorType, ImageEncoder};
use koshi_image::{
    compute_rgba_byte_count, validate_image_dimensions, DecodedImage, GraphicsError,
    GraphicsProtocol, MAX_GRAPHICS_CONTROL_BYTE_COUNT, MAX_GRAPHICS_TRANSFER_BYTE_COUNT,
};

const ITERM2_PROTOCOL: GraphicsProtocol = GraphicsProtocol::Iterm2;
const ITERM_OSC_PREFIX_BYTES: &[u8] = b"\x1b]1337;";
const ITERM_OSC_STRING_TERMINATOR_BYTES: &[u8] = b"\x1b\\";
/// The largest complete OSC packet emitted by the encoder.
pub const MAX_ITERM_PACKET_BYTE_COUNT: usize = 64 * 1024;
const MAX_ITERM_PNG_BYTE_COUNT: usize = (MAX_GRAPHICS_TRANSFER_BYTE_COUNT / 4) * 3;
const ITERM_FILE_PART_PREFIX_BYTES: &[u8] = b"FilePart=";
const MAX_FILE_PART_BASE64_BYTE_COUNT: usize = ((MAX_ITERM_PACKET_BYTE_COUNT
    - ITERM_OSC_PREFIX_BYTES.len()
    - ITERM_FILE_PART_PREFIX_BYTES.len()
    - ITERM_OSC_STRING_TERMINATOR_BYTES.len())
    / 4)
    * 4;
const MAX_FILE_PART_RAW_BYTE_COUNT: usize = MAX_FILE_PART_BASE64_BYTE_COUNT / 4 * 3;

/// Requested cell dimensions for an encoded iTerm2 image.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ItermOutputOptions {
    /// Requested number of terminal columns. [`ItermEncoder::from_image`] validates this
    /// value when it starts an output transfer.
    pub width_cells: u32,
    /// Requested number of terminal rows. [`ItermEncoder::from_image`] validates this
    /// value when it starts an output transfer.
    pub height_cells: u32,
}

impl ItermOutputOptions {
    /// Validate and create output dimensions.
    pub fn from_cell_dimensions(
        width_cells: u32,
        height_cells: u32,
    ) -> Result<Self, GraphicsError> {
        if width_cells == 0 || height_cells == 0 {
            return Err(GraphicsError::InvalidDimensions {
                protocol: ITERM2_PROTOCOL,
            });
        }
        Ok(Self {
            width_cells,
            height_cells,
        })
    }
}

/// Errors returned while constructing or emitting an iTerm2 image command.
#[derive(Debug)]
pub enum ItermEncodeError {
    /// The image or output dimensions violate the shared graphics limits.
    Graphics(GraphicsError),
    /// The PNG encoder rejected validated RGBA input.
    Png(image::ImageError),
    /// The destination writer rejected output.
    Io(io::Error),
}

impl std::fmt::Display for ItermEncodeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Graphics(graphics_error) => graphics_error.fmt(formatter),
            Self::Png(png_error) => write!(formatter, "iTerm2 PNG encoding failed: {png_error}"),
            Self::Io(io_error) => write!(formatter, "iTerm2 image output failed: {io_error}"),
        }
    }
}

impl std::error::Error for ItermEncodeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Graphics(graphics_error) => Some(graphics_error),
            Self::Png(png_error) => Some(png_error),
            Self::Io(io_error) => Some(io_error),
        }
    }
}

impl From<GraphicsError> for ItermEncodeError {
    fn from(graphics_error: GraphicsError) -> Self {
        Self::Graphics(graphics_error)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TransferMode {
    File,
    Multipart,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PacketPhase {
    File,
    MultipartHeader,
    MultipartPart,
    MultipartEnd,
    Complete,
}

/// A reusable encoder that emits complete, independently terminated OSC 1337
/// packets.
pub struct ItermEncoder {
    encoded_png_bytes: Vec<u8>,
    metadata_bytes: Vec<u8>,
    packet_bytes: Vec<u8>,
    png_byte_offset: usize,
    transfer_mode: TransferMode,
    packet_phase: PacketPhase,
}

impl std::fmt::Debug for ItermEncoder {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ItermEncoder")
            .field("png_byte_count", &self.encoded_png_bytes.len())
            .field("metadata_byte_count", &self.metadata_bytes.len())
            .field("packet_byte_count", &self.packet_bytes.len())
            .field("png_byte_offset", &self.png_byte_offset)
            .field("transfer_mode", &self.transfer_mode)
            .field("packet_phase", &self.packet_phase)
            .finish()
    }
}

impl ItermEncoder {
    /// Prepare validated RGBA pixels as one `File` packet or multipart
    /// packets. Construction performs PNG encoding but does not write output.
    pub fn from_image(
        decoded_image: &DecodedImage,
        output_options: ItermOutputOptions,
    ) -> Result<Self, ItermEncodeError> {
        validate_iterm_output_options(output_options)?;
        let encoded_png_bytes = encode_png(decoded_image)?;
        let metadata_bytes = build_iterm_metadata_bytes(output_options, encoded_png_bytes.len());
        if metadata_bytes.len() > MAX_GRAPHICS_CONTROL_BYTE_COUNT {
            return Err(GraphicsError::TransferTooLarge {
                protocol: ITERM2_PROTOCOL,
            }
            .into());
        }

        let encoded_png_byte_count = compute_base64_encoded_byte_count(encoded_png_bytes.len())?;
        let file_body_byte_count = compute_checked_byte_count_sum(&[
            b"File=".len(),
            metadata_bytes.len(),
            1,
            encoded_png_byte_count,
        ])?;
        let file_packet_byte_count = compute_framed_byte_count(file_body_byte_count)?;
        let transfer_mode = if file_packet_byte_count <= MAX_ITERM_PACKET_BYTE_COUNT {
            TransferMode::File
        } else {
            let multipart_body_byte_count =
                compute_checked_byte_count_sum(&[b"MultipartFile=".len(), metadata_bytes.len()])?;
            if compute_framed_byte_count(multipart_body_byte_count)? > MAX_ITERM_PACKET_BYTE_COUNT {
                return Err(GraphicsError::TransferTooLarge {
                    protocol: ITERM2_PROTOCOL,
                }
                .into());
            }
            TransferMode::Multipart
        };
        Ok(Self {
            encoded_png_bytes,
            metadata_bytes,
            packet_bytes: Vec::with_capacity(MAX_ITERM_PACKET_BYTE_COUNT),
            png_byte_offset: 0,
            transfer_mode,
            packet_phase: match transfer_mode {
                TransferMode::File => PacketPhase::File,
                TransferMode::Multipart => PacketPhase::MultipartHeader,
            },
        })
    }

    /// Return the next complete terminated OSC 1337 packet.
    ///
    /// The returned slice remains valid until the next mutable operation on
    /// this encoder. A packet is consumed when this method returns it.
    pub fn take_next_packet(&mut self) -> Option<&[u8]> {
        if self.packet_phase == PacketPhase::Complete {
            return None;
        }
        self.build_packet();
        self.advance_packet_phase();
        Some(&self.packet_bytes)
    }

    /// Write one complete packet and report whether a packet was written.
    ///
    /// The phase does not advance when `write_all` returns an error. A writer
    /// that reports a partial write may have received part of the packet; the
    /// caller must discard that transport and restart the encoder before
    /// retrying.
    pub fn write_next_packet<W: Write>(&mut self, mut writer: W) -> Result<bool, ItermEncodeError> {
        if self.packet_phase == PacketPhase::Complete {
            return Ok(false);
        }
        self.build_packet();
        writer
            .write_all(&self.packet_bytes)
            .map_err(ItermEncodeError::Io)?;
        self.advance_packet_phase();
        Ok(true)
    }

    /// Reset packet emission to the beginning of this image transfer.
    pub fn reset_packet_emission(&mut self) {
        self.png_byte_offset = 0;
        self.packet_bytes.clear();
        self.packet_phase = match self.transfer_mode {
            TransferMode::File => PacketPhase::File,
            TransferMode::Multipart => PacketPhase::MultipartHeader,
        };
    }

    /// Return whether every packet has been returned or written.
    pub fn is_complete(&self) -> bool {
        self.packet_phase == PacketPhase::Complete
    }

    fn build_packet(&mut self) {
        self.packet_bytes.clear();
        self.packet_bytes.extend_from_slice(ITERM_OSC_PREFIX_BYTES);
        // The constructor bounds the PNG and metadata; each phase has a fixed packet budget.
        match self.packet_phase {
            PacketPhase::File => {
                self.packet_bytes.extend_from_slice(b"File=");
                self.packet_bytes.extend_from_slice(&self.metadata_bytes);
                self.packet_bytes.push(b':');
                append_base64_bytes(&mut self.packet_bytes, &self.encoded_png_bytes);
            }
            PacketPhase::MultipartHeader => {
                self.packet_bytes.extend_from_slice(b"MultipartFile=");
                self.packet_bytes.extend_from_slice(&self.metadata_bytes);
            }
            PacketPhase::MultipartPart => {
                self.packet_bytes
                    .extend_from_slice(ITERM_FILE_PART_PREFIX_BYTES);
                let remaining_png_byte_count = self.encoded_png_bytes.len() - self.png_byte_offset;
                let raw_byte_count = remaining_png_byte_count.min(MAX_FILE_PART_RAW_BYTE_COUNT);
                append_base64_bytes(
                    &mut self.packet_bytes,
                    &self.encoded_png_bytes
                        [self.png_byte_offset..self.png_byte_offset + raw_byte_count],
                );
            }
            PacketPhase::MultipartEnd => self.packet_bytes.extend_from_slice(b"FileEnd"),
            PacketPhase::Complete => return,
        }
        self.packet_bytes
            .extend_from_slice(ITERM_OSC_STRING_TERMINATOR_BYTES);
        debug_assert!(self.packet_bytes.len() <= MAX_ITERM_PACKET_BYTE_COUNT);
    }

    fn advance_packet_phase(&mut self) {
        self.packet_phase = match self.packet_phase {
            PacketPhase::File => PacketPhase::Complete,
            PacketPhase::MultipartHeader => PacketPhase::MultipartPart,
            PacketPhase::MultipartPart => {
                let remaining_png_byte_count = self.encoded_png_bytes.len() - self.png_byte_offset;
                let raw_byte_count = remaining_png_byte_count.min(MAX_FILE_PART_RAW_BYTE_COUNT);
                self.png_byte_offset += raw_byte_count;
                if self.png_byte_offset == self.encoded_png_bytes.len() {
                    PacketPhase::MultipartEnd
                } else {
                    PacketPhase::MultipartPart
                }
            }
            PacketPhase::MultipartEnd => PacketPhase::Complete,
            PacketPhase::Complete => PacketPhase::Complete,
        };
    }
}

fn validate_iterm_output_options(
    output_options: ItermOutputOptions,
) -> Result<(), ItermEncodeError> {
    ItermOutputOptions::from_cell_dimensions(
        output_options.width_cells,
        output_options.height_cells,
    )
    .map(|_| ())?;
    Ok(())
}

fn build_iterm_metadata_bytes(
    output_options: ItermOutputOptions,
    png_byte_count: usize,
) -> Vec<u8> {
    format!(
        "inline=1;width={};height={};preserveAspectRatio=0;size={}",
        output_options.width_cells, output_options.height_cells, png_byte_count
    )
    .into_bytes()
}

fn compute_base64_encoded_byte_count(raw_byte_count: usize) -> Result<usize, ItermEncodeError> {
    if raw_byte_count > MAX_ITERM_PNG_BYTE_COUNT {
        return Err(GraphicsError::TransferTooLarge {
            protocol: ITERM2_PROTOCOL,
        }
        .into());
    }
    let encoded_output_byte_count = base64::encoded_len(raw_byte_count, true).ok_or_else(|| {
        ItermEncodeError::Graphics(GraphicsError::InvalidDimensions {
            protocol: ITERM2_PROTOCOL,
        })
    })?;
    if encoded_output_byte_count > MAX_GRAPHICS_TRANSFER_BYTE_COUNT {
        return Err(GraphicsError::TransferTooLarge {
            protocol: ITERM2_PROTOCOL,
        }
        .into());
    }
    Ok(encoded_output_byte_count)
}

fn compute_checked_byte_count_sum(byte_counts: &[usize]) -> Result<usize, ItermEncodeError> {
    byte_counts
        .iter()
        .try_fold(0usize, |total_byte_count, byte_count| {
            total_byte_count.checked_add(*byte_count).ok_or_else(|| {
                GraphicsError::InvalidDimensions {
                    protocol: ITERM2_PROTOCOL,
                }
                .into()
            })
        })
}

fn compute_framed_byte_count(body_byte_count: usize) -> Result<usize, ItermEncodeError> {
    compute_checked_byte_count_sum(&[
        ITERM_OSC_PREFIX_BYTES.len(),
        body_byte_count,
        ITERM_OSC_STRING_TERMINATOR_BYTES.len(),
    ])
}

fn append_base64_bytes(packet_bytes: &mut Vec<u8>, raw_bytes: &[u8]) {
    let encoded_output_byte_count = compute_bounded_base64_encoded_byte_count(raw_bytes.len());
    let encoded_start_index = packet_bytes.len();
    let encoded_end_index = encoded_start_index + encoded_output_byte_count;
    debug_assert!(
        encoded_end_index + ITERM_OSC_STRING_TERMINATOR_BYTES.len() <= MAX_ITERM_PACKET_BYTE_COUNT
    );
    packet_bytes.resize(encoded_end_index, 0);
    let written_byte_count = STANDARD
        .encode_slice(raw_bytes, &mut packet_bytes[encoded_start_index..])
        .unwrap_or_else(|_| unreachable!("validated packet has enough base64 space"));
    debug_assert_eq!(written_byte_count, encoded_output_byte_count);
}

fn compute_bounded_base64_encoded_byte_count(raw_byte_count: usize) -> usize {
    debug_assert!(raw_byte_count <= MAX_ITERM_PNG_BYTE_COUNT);
    base64::encoded_len(raw_byte_count, true)
        .unwrap_or_else(|| unreachable!("validated PNG length fits base64 length"))
}

fn encode_png(decoded_image: &DecodedImage) -> Result<Vec<u8>, ItermEncodeError> {
    let column_count =
        usize::try_from(decoded_image.pixel_width).map_err(|_| GraphicsError::ImageTooLarge {
            protocol: ITERM2_PROTOCOL,
        })?;
    let row_count =
        usize::try_from(decoded_image.pixel_height).map_err(|_| GraphicsError::ImageTooLarge {
            protocol: ITERM2_PROTOCOL,
        })?;
    validate_image_dimensions(ITERM2_PROTOCOL, column_count, row_count)?;
    let expected_rgba_byte_count =
        compute_rgba_byte_count(ITERM2_PROTOCOL, column_count, row_count)?;
    if decoded_image.rgba_bytes.len() != expected_rgba_byte_count {
        return Err(GraphicsError::DeclaredSizeMismatch {
            protocol: ITERM2_PROTOCOL,
            expected_byte_count: expected_rgba_byte_count,
            actual_byte_count: decoded_image.rgba_bytes.len(),
        }
        .into());
    }

    let mut encoded_png_output = BoundedByteBuffer::with_byte_limit(MAX_ITERM_PNG_BYTE_COUNT);
    let encode_result = PngEncoder::new_with_quality(
        &mut encoded_png_output,
        CompressionType::Fast,
        FilterType::NoFilter,
    )
    .write_image(
        &decoded_image.rgba_bytes,
        decoded_image.pixel_width,
        decoded_image.pixel_height,
        ExtendedColorType::Rgba8,
    );
    encode_result.map_err(|png_error| {
        if encoded_png_output.has_overflowed() {
            ItermEncodeError::Graphics(GraphicsError::TransferTooLarge {
                protocol: ITERM2_PROTOCOL,
            })
        } else {
            ItermEncodeError::Png(png_error)
        }
    })?;
    Ok(encoded_png_output.into_encoded_output_bytes())
}

struct BoundedByteBuffer {
    encoded_output_bytes: Vec<u8>,
    maximum_byte_count: usize,
    has_overflowed: bool,
}

impl BoundedByteBuffer {
    fn with_byte_limit(maximum_byte_count: usize) -> Self {
        Self {
            encoded_output_bytes: Vec::new(),
            maximum_byte_count,
            has_overflowed: false,
        }
    }

    fn has_overflowed(&self) -> bool {
        self.has_overflowed
    }

    fn into_encoded_output_bytes(self) -> Vec<u8> {
        self.encoded_output_bytes
    }
}

impl Write for BoundedByteBuffer {
    fn write(&mut self, encoded_bytes: &[u8]) -> io::Result<usize> {
        let available_byte_count = self
            .maximum_byte_count
            .saturating_sub(self.encoded_output_bytes.len());
        if encoded_bytes.len() > available_byte_count {
            self.has_overflowed = true;
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "PNG output exceeds graphics transfer limit",
            ));
        }
        self.encoded_output_bytes.extend_from_slice(encoded_bytes);
        Ok(encoded_bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests;
