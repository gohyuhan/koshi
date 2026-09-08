//! Bounded iTerm2 OSC 1337 image output.

use std::io::{self, Write};

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use image::codecs::png::{CompressionType, FilterType, PngEncoder};
use image::{ExtendedColorType, ImageEncoder};
use koshi_image::{
    checked_rgba_len, validate_dimensions, DecodedImage, GraphicsError, GraphicsProtocol,
    MAX_GRAPHICS_CONTROL_BYTES, MAX_GRAPHICS_TRANSFER_BYTES,
};

const ITERM_PROTOCOL: GraphicsProtocol = GraphicsProtocol::Iterm2;
const OSC_PREFIX: &[u8] = b"\x1b]1337;";
const OSC_ST: &[u8] = b"\x1b\\";
/// The largest complete OSC packet emitted by the encoder.
pub const MAX_ITERM_PACKET_BYTES: usize = 64 * 1024;
const MAX_PACKET_BYTES: usize = MAX_ITERM_PACKET_BYTES;
const MAX_PNG_BYTES: usize = (MAX_GRAPHICS_TRANSFER_BYTES / 4) * 3;
const FILE_PART_PREFIX: &[u8] = b"FilePart=";
const MAX_FILE_PART_BASE64_BYTES: usize =
    ((MAX_PACKET_BYTES - OSC_PREFIX.len() - FILE_PART_PREFIX.len() - OSC_ST.len()) / 4) * 4;
const MAX_FILE_PART_RAW_BYTES: usize = MAX_FILE_PART_BASE64_BYTES / 4 * 3;

/// Requested cell dimensions for an encoded iTerm2 image.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutputOptions {
    /// Requested number of terminal columns. [`Encoder::new`] validates this
    /// value when it starts an output transfer.
    pub width_cells: u32,
    /// Requested number of terminal rows. [`Encoder::new`] validates this
    /// value when it starts an output transfer.
    pub height_cells: u32,
}

impl OutputOptions {
    /// Validate and create output dimensions.
    pub fn new(width_cells: u32, height_cells: u32) -> Result<Self, GraphicsError> {
        if width_cells == 0 || height_cells == 0 {
            return Err(GraphicsError::InvalidDimensions {
                protocol: ITERM_PROTOCOL,
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
pub enum EncodeError {
    /// The image or output dimensions violate the shared graphics limits.
    Graphics(GraphicsError),
    /// The PNG encoder rejected validated RGBA input.
    Png(image::ImageError),
    /// The destination writer rejected output.
    Io(io::Error),
}

impl std::fmt::Display for EncodeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Graphics(error) => error.fmt(formatter),
            Self::Png(error) => write!(formatter, "iTerm2 PNG encoding failed: {error}"),
            Self::Io(error) => write!(formatter, "iTerm2 image output failed: {error}"),
        }
    }
}

impl std::error::Error for EncodeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Graphics(error) => Some(error),
            Self::Png(error) => Some(error),
            Self::Io(error) => Some(error),
        }
    }
}

impl From<GraphicsError> for EncodeError {
    fn from(error: GraphicsError) -> Self {
        Self::Graphics(error)
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
pub struct Encoder {
    png: Vec<u8>,
    metadata: Vec<u8>,
    packet: Vec<u8>,
    png_offset: usize,
    mode: TransferMode,
    phase: PacketPhase,
}

impl std::fmt::Debug for Encoder {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Encoder")
            .field("png_len", &self.png.len())
            .field("metadata_len", &self.metadata.len())
            .field("packet_len", &self.packet.len())
            .field("png_offset", &self.png_offset)
            .field("mode", &self.mode)
            .field("phase", &self.phase)
            .finish()
    }
}

impl Encoder {
    /// Prepare validated RGBA pixels as one `File` packet or multipart
    /// packets. Construction performs PNG encoding but does not write output.
    pub fn new(image: &DecodedImage, options: OutputOptions) -> Result<Self, EncodeError> {
        validate_output_options(options)?;
        let png = encode_png(image)?;
        let metadata = build_metadata(options, png.len());
        if metadata.len() > MAX_GRAPHICS_CONTROL_BYTES {
            return Err(GraphicsError::TransferTooLarge {
                protocol: ITERM_PROTOCOL,
            }
            .into());
        }

        let encoded_len = base64_encoded_len(png.len())?;
        let file_body_len = checked_sum(&[b"File=".len(), metadata.len(), 1, encoded_len])?;
        let file_packet_len = framed_len(file_body_len)?;
        let mode = if file_packet_len <= MAX_PACKET_BYTES {
            TransferMode::File
        } else {
            let multipart_body_len = checked_sum(&[b"MultipartFile=".len(), metadata.len()])?;
            if framed_len(multipart_body_len)? > MAX_PACKET_BYTES {
                return Err(GraphicsError::TransferTooLarge {
                    protocol: ITERM_PROTOCOL,
                }
                .into());
            }
            TransferMode::Multipart
        };
        Ok(Self {
            png,
            metadata,
            packet: Vec::with_capacity(MAX_PACKET_BYTES),
            png_offset: 0,
            mode,
            phase: match mode {
                TransferMode::File => PacketPhase::File,
                TransferMode::Multipart => PacketPhase::MultipartHeader,
            },
        })
    }

    /// Return the next complete terminated OSC 1337 packet.
    ///
    /// The returned slice remains valid until the next mutable operation on
    /// this encoder. A packet is consumed when this method returns it.
    pub fn next_packet(&mut self) -> Option<&[u8]> {
        if self.phase == PacketPhase::Complete {
            return None;
        }
        self.build_packet();
        self.advance_phase();
        Some(&self.packet)
    }

    /// Write one complete packet and report whether a packet was written.
    ///
    /// The phase does not advance when `write_all` returns an error. A writer
    /// that reports a partial write may have received part of the packet; the
    /// caller must discard that transport and restart the encoder before
    /// retrying.
    pub fn write_next_packet<W: Write>(&mut self, mut writer: W) -> Result<bool, EncodeError> {
        if self.phase == PacketPhase::Complete {
            return Ok(false);
        }
        self.build_packet();
        writer.write_all(&self.packet).map_err(EncodeError::Io)?;
        self.advance_phase();
        Ok(true)
    }

    /// Reset packet emission to the beginning of this image transfer.
    pub fn reset(&mut self) {
        self.png_offset = 0;
        self.packet.clear();
        self.phase = match self.mode {
            TransferMode::File => PacketPhase::File,
            TransferMode::Multipart => PacketPhase::MultipartHeader,
        };
    }

    /// Return whether every packet has been returned or written.
    pub fn is_complete(&self) -> bool {
        self.phase == PacketPhase::Complete
    }

    fn build_packet(&mut self) {
        self.packet.clear();
        self.packet.extend_from_slice(OSC_PREFIX);
        // The constructor bounds the PNG and metadata; each phase has a fixed packet budget.
        match self.phase {
            PacketPhase::File => {
                self.packet.extend_from_slice(b"File=");
                self.packet.extend_from_slice(&self.metadata);
                self.packet.push(b':');
                append_base64(&mut self.packet, &self.png);
            }
            PacketPhase::MultipartHeader => {
                self.packet.extend_from_slice(b"MultipartFile=");
                self.packet.extend_from_slice(&self.metadata);
            }
            PacketPhase::MultipartPart => {
                self.packet.extend_from_slice(FILE_PART_PREFIX);
                let remaining = self.png.len() - self.png_offset;
                let raw_len = remaining.min(MAX_FILE_PART_RAW_BYTES);
                append_base64(
                    &mut self.packet,
                    &self.png[self.png_offset..self.png_offset + raw_len],
                );
            }
            PacketPhase::MultipartEnd => self.packet.extend_from_slice(b"FileEnd"),
            PacketPhase::Complete => return,
        }
        self.packet.extend_from_slice(OSC_ST);
        debug_assert!(self.packet.len() <= MAX_PACKET_BYTES);
    }

    fn advance_phase(&mut self) {
        self.phase = match self.phase {
            PacketPhase::File => PacketPhase::Complete,
            PacketPhase::MultipartHeader => PacketPhase::MultipartPart,
            PacketPhase::MultipartPart => {
                let remaining = self.png.len() - self.png_offset;
                let raw_len = remaining.min(MAX_FILE_PART_RAW_BYTES);
                self.png_offset += raw_len;
                if self.png_offset == self.png.len() {
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

fn validate_output_options(options: OutputOptions) -> Result<(), EncodeError> {
    OutputOptions::new(options.width_cells, options.height_cells).map(|_| ())?;
    Ok(())
}

fn build_metadata(options: OutputOptions, png_len: usize) -> Vec<u8> {
    format!(
        "inline=1;width={};height={};preserveAspectRatio=0;size={}",
        options.width_cells, options.height_cells, png_len
    )
    .into_bytes()
}

fn base64_encoded_len(raw_len: usize) -> Result<usize, EncodeError> {
    if raw_len > MAX_PNG_BYTES {
        return Err(GraphicsError::TransferTooLarge {
            protocol: ITERM_PROTOCOL,
        }
        .into());
    }
    let encoded_len = base64::encoded_len(raw_len, true).ok_or_else(|| {
        EncodeError::Graphics(GraphicsError::InvalidDimensions {
            protocol: ITERM_PROTOCOL,
        })
    })?;
    if encoded_len > MAX_GRAPHICS_TRANSFER_BYTES {
        return Err(GraphicsError::TransferTooLarge {
            protocol: ITERM_PROTOCOL,
        }
        .into());
    }
    Ok(encoded_len)
}

fn checked_sum(values: &[usize]) -> Result<usize, EncodeError> {
    values.iter().try_fold(0usize, |total, value| {
        total.checked_add(*value).ok_or_else(|| {
            GraphicsError::InvalidDimensions {
                protocol: ITERM_PROTOCOL,
            }
            .into()
        })
    })
}

fn framed_len(body_len: usize) -> Result<usize, EncodeError> {
    checked_sum(&[OSC_PREFIX.len(), body_len, OSC_ST.len()])
}

fn append_base64(packet: &mut Vec<u8>, raw: &[u8]) {
    let encoded_len = bounded_base64_encoded_len(raw.len());
    let encoded_start = packet.len();
    let encoded_end = encoded_start + encoded_len;
    debug_assert!(encoded_end + OSC_ST.len() <= MAX_PACKET_BYTES);
    packet.resize(encoded_end, 0);
    let written = STANDARD
        .encode_slice(raw, &mut packet[encoded_start..])
        .unwrap_or_else(|_| unreachable!("validated packet has enough base64 space"));
    debug_assert_eq!(written, encoded_len);
}

fn bounded_base64_encoded_len(raw_len: usize) -> usize {
    debug_assert!(raw_len <= MAX_PNG_BYTES);
    base64::encoded_len(raw_len, true)
        .unwrap_or_else(|| unreachable!("validated PNG length fits base64 length"))
}

fn encode_png(image: &DecodedImage) -> Result<Vec<u8>, EncodeError> {
    let width = usize::try_from(image.width).map_err(|_| GraphicsError::ImageTooLarge {
        protocol: ITERM_PROTOCOL,
    })?;
    let height = usize::try_from(image.height).map_err(|_| GraphicsError::ImageTooLarge {
        protocol: ITERM_PROTOCOL,
    })?;
    validate_dimensions(ITERM_PROTOCOL, width, height)?;
    let expected = checked_rgba_len(ITERM_PROTOCOL, width, height)?;
    if image.rgba.len() != expected {
        return Err(GraphicsError::DeclaredSizeMismatch {
            protocol: ITERM_PROTOCOL,
            expected,
            actual: image.rgba.len(),
        }
        .into());
    }

    let mut output = BoundedVec::new(MAX_PNG_BYTES);
    let result =
        PngEncoder::new_with_quality(&mut output, CompressionType::Fast, FilterType::NoFilter)
            .write_image(
                &image.rgba,
                image.width,
                image.height,
                ExtendedColorType::Rgba8,
            );
    result.map_err(|error| {
        if output.overflowed() {
            EncodeError::Graphics(GraphicsError::TransferTooLarge {
                protocol: ITERM_PROTOCOL,
            })
        } else {
            EncodeError::Png(error)
        }
    })?;
    Ok(output.into_inner())
}

struct BoundedVec {
    bytes: Vec<u8>,
    limit: usize,
    overflowed: bool,
}

impl BoundedVec {
    fn new(limit: usize) -> Self {
        Self {
            bytes: Vec::new(),
            limit,
            overflowed: false,
        }
    }

    fn overflowed(&self) -> bool {
        self.overflowed
    }

    fn into_inner(self) -> Vec<u8> {
        self.bytes
    }
}

impl Write for BoundedVec {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let available = self.limit.saturating_sub(self.bytes.len());
        if bytes.len() > available {
            self.overflowed = true;
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "PNG output exceeds graphics transfer limit",
            ));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests;
