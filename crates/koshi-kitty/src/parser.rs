//! Kitty APC parsing, transfer validation, and image decoding.

#[cfg(unix)]
use std::fs::OpenOptions;
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
#[cfg(unix)]
use std::ffi::CString;
#[cfg(unix)]
use std::os::fd::FromRawFd;

use koshi_image::{
    decode_base64, decode_png, decode_raw_rgb, decode_raw_rgba, decompress_bounded,
    decompress_bounded_prefix, validate_image_dimensions, DecodedGraphics, GraphicsError,
    GraphicsProtocol, ImageAction, ImageDisplay, MAX_GRAPHICS_CONTROL_BYTE_COUNT,
    MAX_GRAPHICS_TRANSFER_BYTE_COUNT,
};

const KITTY_PROTOCOL: GraphicsProtocol = GraphicsProtocol::Kitty;
const PNG_SIGNATURE_BYTES: &[u8; 8] = b"\x89PNG\r\n\x1a\n";

mod commands;

pub use commands::{
    parse_reply_display, KittyAnimationChunk, KittyAnimationCommand, KittyCommand,
    KittyCommandKind, KittyDelete,
};

/// Parse one non-multipart Kitty command and prepare animation-frame payload.
///
/// Returns `None` for image transfers and multipart animation-frame starts.
pub fn parse_kitty_command(
    control_header_bytes: &[u8],
    encoded_payload_bytes: &[u8],
) -> Option<Result<KittyCommand, GraphicsError>> {
    let action_code = control_header_bytes
        .strip_prefix(b"G")?
        .split(|byte| *byte == b',')
        .find_map(|control_field| control_field.strip_prefix(b"a="));
    let parsed_command =
        commands::parse_kitty_command(control_header_bytes, encoded_payload_bytes)?;
    if action_code != Some(b"f") {
        return Some(parsed_command);
    }
    Some(parsed_command.and_then(|_| {
        let prepared_payload_bytes =
            prepare_animation_payload(control_header_bytes, encoded_payload_bytes, true)?;
        let control_body_bytes =
            control_header_bytes
                .strip_prefix(b"G")
                .ok_or(GraphicsError::InvalidHeader {
                    protocol: KITTY_PROTOCOL,
                })?;
        commands::parse_animation_command_fields(
            control_body_bytes,
            &prepared_payload_bytes,
            KittyCommandKind::AnimationFrame,
            false,
        )
    }))
}

/// The largest base64 payload chunk accepted by the Kitty graphics protocol.
pub const MAX_KITTY_CHUNK_BYTE_COUNT: usize = 4096;

/// Incremental parser for one Kitty APC graphics string.
#[derive(Clone)]
pub struct KittyParser {
    control_header_bytes: Vec<u8>,
    payload_bytes: Vec<u8>,
    has_received_control_header: bool,
    is_ignored: bool,
    is_escaped: bool,
}

impl KittyParser {
    /// Create an empty Kitty APC parser.
    #[must_use]
    pub fn new() -> Self {
        Self {
            control_header_bytes: Vec::new(),
            payload_bytes: Vec::new(),
            has_received_control_header: false,
            is_ignored: false,
            is_escaped: false,
        }
    }

    /// Feed one byte from the APC body.
    pub fn feed_input_byte(&mut self, input_byte: u8) -> Result<(), GraphicsError> {
        if self.is_ignored {
            return Ok(());
        }
        if !self.has_received_control_header {
            if self.control_header_bytes.is_empty() && input_byte != b'G' {
                self.is_ignored = true;
                return Ok(());
            }
            if self.control_header_bytes.is_empty() {
                self.control_header_bytes.push(input_byte);
                return Ok(());
            }
            if input_byte == b';' {
                self.has_received_control_header = true;
                return Ok(());
            }
            push_bounded_bytes(
                &mut self.control_header_bytes,
                input_byte,
                MAX_GRAPHICS_CONTROL_BYTE_COUNT,
            )?;
        } else {
            push_bounded_bytes(
                &mut self.payload_bytes,
                input_byte,
                MAX_KITTY_CHUNK_BYTE_COUNT,
            )?;
        }
        Ok(())
    }

    /// Return whether the APC body was identified as a non-graphics command.
    #[must_use]
    pub fn is_ignored(&self) -> bool {
        self.is_ignored
    }

    /// Return whether the framing parser has received an escape byte.
    #[must_use]
    pub fn is_escaped(&self) -> bool {
        self.is_escaped
    }

    /// Set the framing parser's escape-byte state.
    pub fn set_escaped(&mut self, is_escaped: bool) {
        self.is_escaped = is_escaped;
    }

    /// Return whether the control-payload separator has been received.
    #[must_use]
    pub fn has_received_control_header(&self) -> bool {
        self.has_received_control_header
    }

    /// Return the bytes received before the control-payload separator.
    #[must_use]
    pub fn get_control_header_bytes(&self) -> &[u8] {
        &self.control_header_bytes
    }

    /// Return the base64 bytes received after the control-payload separator.
    #[must_use]
    pub fn get_payload_bytes(&self) -> &[u8] {
        &self.payload_bytes
    }

    /// Return the number of payload bytes received in this APC string.
    #[must_use]
    pub fn get_payload_byte_count(&self) -> usize {
        self.payload_bytes.len()
    }

    /// Append a validated run of base64 payload bytes.
    pub fn append_payload_bytes(&mut self, payload_bytes: &[u8]) -> Result<(), GraphicsError> {
        let new_payload_byte_count = self
            .payload_bytes
            .len()
            .checked_add(payload_bytes.len())
            .ok_or(GraphicsError::TransferTooLarge {
                protocol: KITTY_PROTOCOL,
            })?;
        if new_payload_byte_count > MAX_KITTY_CHUNK_BYTE_COUNT {
            return Err(GraphicsError::TransferTooLarge {
                protocol: KITTY_PROTOCOL,
            });
        }
        self.payload_bytes.extend_from_slice(payload_bytes);
        Ok(())
    }

    /// Finish parsing the control bytes and payload as one Kitty chunk.
    pub fn finish_kitty_chunk(self) -> Result<KittyChunk, GraphicsError> {
        if !self.has_received_control_header
            || self.control_header_bytes.first().copied() != Some(b'G')
        {
            return Err(GraphicsError::InvalidHeader {
                protocol: KITTY_PROTOCOL,
            });
        }
        let kitty_control = parse_kitty_control(&self.control_header_bytes[1..])?;
        KittyChunk::from_kitty_control(kitty_control, self.payload_bytes)
    }

    /// Finish parsing a multipart Kitty animation-frame chunk.
    ///
    /// Returns `None` when the APC is not an animation-frame chunk.
    pub fn finish_kitty_animation_chunk(
        self,
    ) -> Result<Option<KittyAnimationChunk>, GraphicsError> {
        if !self.has_received_control_header
            || self.control_header_bytes.first().copied() != Some(b'G')
        {
            return Err(GraphicsError::InvalidHeader {
                protocol: KITTY_PROTOCOL,
            });
        }
        commands::parse_animation_transfer_chunk(&self.control_header_bytes, &self.payload_bytes)
    }
}

impl Default for KittyParser {
    fn default() -> Self {
        Self::new()
    }
}

/// A validated Kitty image chunk.
#[derive(Clone)]
pub struct KittyChunk {
    is_query: bool,
    image_id: Option<u32>,
    action: ImageAction,
    has_more_chunks: bool,
    has_more_chunks_parameter: bool,
    supports_continuation: bool,
    media_format: Option<KittyFormat>,
    image_width_pixels: Option<u32>,
    image_height_pixels: Option<u32>,
    image_display: ImageDisplay,
    transfer_medium: u8,
    encoded_payload_bytes: Vec<u8>,
    is_raw_payload: bool,
    is_compressed: Option<bool>,
    declared_payload_byte_count: Option<usize>,
}

impl KittyChunk {
    fn from_kitty_control(
        kitty_control: KittyControl,
        encoded_payload_bytes: Vec<u8>,
    ) -> Result<Self, GraphicsError> {
        if encoded_payload_bytes.len() > MAX_KITTY_CHUNK_BYTE_COUNT {
            return Err(GraphicsError::TransferTooLarge {
                protocol: KITTY_PROTOCOL,
            });
        }
        if kitty_control.has_more_chunks
            && (!encoded_payload_bytes.len().is_multiple_of(4)
                || encoded_payload_bytes.contains(&b'='))
        {
            return Err(GraphicsError::InvalidBase64 {
                protocol: KITTY_PROTOCOL,
            });
        }
        let transfer_medium = kitty_control.transfer_medium.unwrap_or(b'd');
        let is_continuation =
            kitty_control.has_more_chunks_parameter && kitty_control.supports_continuation;
        let (encoded_payload_bytes, is_raw_payload, is_compressed, declared_payload_byte_count) =
            if is_continuation {
                if transfer_medium != b'd' || kitty_control.source_byte_offset.is_some() {
                    return Err(GraphicsError::InvalidCommand {
                        protocol: KITTY_PROTOCOL,
                    });
                }
                (
                    encoded_payload_bytes,
                    false,
                    kitty_control.is_compressed,
                    kitty_control.source_byte_count,
                )
            } else {
                validate_transfer_controls(&kitty_control, transfer_medium)?;
                let transfer_payload = load_transfer_payload(TransferRequest {
                    transfer_medium,
                    source_payload_bytes: &encoded_payload_bytes,
                    source_byte_offset: kitty_control.source_byte_offset,
                    source_byte_count: kitty_control.source_byte_count,
                    is_compressed: kitty_control.is_compressed == Some(true),
                    is_final_chunk: !kitty_control.has_more_chunks,
                    media_format: kitty_control.media_format,
                    image_width_pixels: kitty_control.image_width_pixels,
                    image_height_pixels: kitty_control.image_height_pixels,
                })?;
                match transfer_payload {
                    TransferData::EncodedPayloadBytes(encoded_payload_bytes) => (
                        encoded_payload_bytes,
                        false,
                        kitty_control.is_compressed,
                        kitty_control.source_byte_count,
                    ),
                    TransferData::DecodedPayloadBytes(decoded_payload_bytes) => {
                        (decoded_payload_bytes, true, Some(false), None)
                    }
                }
            };
        if matches!(
            kitty_control.media_format,
            Some(KittyFormat::Rgb | KittyFormat::Rgba)
        ) && (kitty_control.image_width_pixels == Some(0)
            || kitty_control.image_height_pixels == Some(0))
        {
            return Err(GraphicsError::InvalidDimensions {
                protocol: KITTY_PROTOCOL,
            });
        }
        Ok(KittyChunk {
            is_query: kitty_control.is_query,
            image_id: kitty_control.image_id,
            action: kitty_control.action,
            has_more_chunks: kitty_control.has_more_chunks,
            has_more_chunks_parameter: kitty_control.has_more_chunks_parameter,
            supports_continuation: kitty_control.supports_continuation,
            media_format: kitty_control.media_format,
            image_width_pixels: kitty_control.image_width_pixels,
            image_height_pixels: kitty_control.image_height_pixels,
            image_display: kitty_control.image_display,
            transfer_medium,
            encoded_payload_bytes,
            is_raw_payload,
            is_compressed,
            declared_payload_byte_count,
        })
    }

    /// Return whether another chunk is required before decoding.
    #[must_use]
    pub fn has_more_chunks(&self) -> bool {
        self.has_more_chunks
    }
}

/// A validated Kitty transfer that is waiting for more chunks.
#[derive(Debug, Clone)]
pub struct KittyTransfer {
    is_query: bool,
    image_id: Option<u32>,
    action: ImageAction,
    media_format: KittyFormat,
    image_width_pixels: Option<u32>,
    image_height_pixels: Option<u32>,
    image_display: ImageDisplay,
    transfer_medium: u8,
    encoded_payload_bytes: Vec<u8>,
    is_raw_payload: bool,
    is_compressed: bool,
    declared_payload_byte_count: Option<usize>,
}

/// The outcome of starting or extending a Kitty transfer.
#[derive(Debug)]
pub enum KittyTransferOutcome {
    /// The transfer remains open and must receive another chunk.
    Pending(KittyTransfer),
    /// The final chunk produced a decoded graphics record.
    Complete(DecodedGraphics),
}

/// A validated Kitty animation-frame transfer that is waiting for more chunks.
#[derive(Debug, Clone)]
pub struct KittyAnimationTransfer {
    control_header_bytes: Vec<u8>,
    encoded_payload_bytes: Vec<u8>,
}

/// The outcome of starting or extending a Kitty animation-frame transfer.
#[derive(Debug)]
pub enum KittyAnimationTransferOutcome {
    /// The transfer remains open and must receive another chunk.
    Pending(KittyAnimationTransfer),
    /// The final chunk produced a validated animation command.
    Complete(Box<KittyCommand>),
}

/// Start a Kitty transfer from its first validated chunk.
pub fn start_kitty_transfer(chunk: KittyChunk) -> Result<KittyTransferOutcome, GraphicsError> {
    let has_more_chunks = chunk.has_more_chunks;
    let transfer = KittyTransfer::from_chunk(chunk)?;
    if has_more_chunks {
        Ok(KittyTransferOutcome::Pending(transfer))
    } else {
        Ok(KittyTransferOutcome::Complete(transfer.decode_graphics()?))
    }
}

/// Start a multipart Kitty animation-frame transfer.
pub fn start_kitty_animation_transfer(
    chunk: KittyAnimationChunk,
) -> Result<KittyAnimationTransferOutcome, GraphicsError> {
    if chunk.is_continuation() || !chunk.has_more_chunks() {
        return Err(GraphicsError::InvalidHeader {
            protocol: KITTY_PROTOCOL,
        });
    }
    let transfer = KittyAnimationTransfer {
        control_header_bytes: chunk.get_control_header_bytes().to_vec(),
        encoded_payload_bytes: chunk.get_encoded_payload_bytes().to_vec(),
    };
    Ok(KittyAnimationTransferOutcome::Pending(transfer))
}

impl KittyTransfer {
    fn from_chunk(chunk: KittyChunk) -> Result<Self, GraphicsError> {
        let media_format = chunk.media_format.unwrap_or(KittyFormat::Rgba);
        let image_width_pixels = chunk.image_width_pixels;
        let image_height_pixels = chunk.image_height_pixels;
        if matches!(media_format, KittyFormat::Rgb | KittyFormat::Rgba)
            && (image_width_pixels.is_none() || image_height_pixels.is_none())
        {
            return Err(GraphicsError::InvalidDimensions {
                protocol: KITTY_PROTOCOL,
            });
        }
        if let (Some(image_width_pixels), Some(image_height_pixels)) =
            (image_width_pixels, image_height_pixels)
        {
            validate_image_dimensions(
                KITTY_PROTOCOL,
                usize::try_from(image_width_pixels).map_err(|_| GraphicsError::ImageTooLarge {
                    protocol: KITTY_PROTOCOL,
                })?,
                usize::try_from(image_height_pixels).map_err(|_| GraphicsError::ImageTooLarge {
                    protocol: KITTY_PROTOCOL,
                })?,
            )?;
        }
        Ok(KittyTransfer {
            is_query: chunk.is_query,
            image_id: chunk.image_id,
            action: chunk.action,
            media_format,
            image_width_pixels,
            image_height_pixels,
            image_display: chunk.image_display,
            transfer_medium: chunk.transfer_medium,
            encoded_payload_bytes: chunk.encoded_payload_bytes,
            is_raw_payload: chunk.is_raw_payload,
            is_compressed: chunk.is_compressed.unwrap_or(false),
            declared_payload_byte_count: chunk.declared_payload_byte_count,
        })
    }

    /// Return display metadata used for a response to this transfer.
    #[must_use]
    pub fn get_image_display(&self) -> &ImageDisplay {
        &self.image_display
    }

    /// Validate and append a continuation chunk.
    pub fn accept_continuation_chunk(
        mut self,
        chunk: KittyChunk,
    ) -> Result<KittyTransferOutcome, GraphicsError> {
        validate_kitty_continuation(&self, &chunk)?;
        append_bounded_bytes(
            &mut self.encoded_payload_bytes,
            &chunk.encoded_payload_bytes,
        )?;
        if chunk.has_more_chunks {
            Ok(KittyTransferOutcome::Pending(self))
        } else {
            if let Some(declared_payload_byte_count) = chunk.declared_payload_byte_count {
                if self.declared_payload_byte_count.is_some()
                    && self.declared_payload_byte_count != Some(declared_payload_byte_count)
                {
                    return Err(GraphicsError::InvalidHeader {
                        protocol: KITTY_PROTOCOL,
                    });
                }
                self.declared_payload_byte_count = Some(declared_payload_byte_count);
            }
            Ok(KittyTransferOutcome::Complete(self.decode_graphics()?))
        }
    }

    fn decode_graphics(self) -> Result<DecodedGraphics, GraphicsError> {
        let decoded_payload_bytes = if self.is_raw_payload {
            self.encoded_payload_bytes
        } else {
            match load_transfer_payload(TransferRequest {
                transfer_medium: self.transfer_medium,
                source_payload_bytes: &self.encoded_payload_bytes,
                source_byte_offset: None,
                source_byte_count: self.declared_payload_byte_count,
                is_compressed: self.is_compressed,
                is_final_chunk: true,
                media_format: Some(self.media_format),
                image_width_pixels: self.image_width_pixels,
                image_height_pixels: self.image_height_pixels,
            })? {
                TransferData::DecodedPayloadBytes(decoded_payload_bytes) => decoded_payload_bytes,
                TransferData::EncodedPayloadBytes(_) => {
                    return Err(GraphicsError::InvalidBase64 {
                        protocol: KITTY_PROTOCOL,
                    })
                }
            }
        };
        let (decoded_image, animation) = match self.media_format {
            KittyFormat::Rgb => (
                decode_raw_rgb(
                    KITTY_PROTOCOL,
                    self.image_width_pixels
                        .ok_or(GraphicsError::InvalidDimensions {
                            protocol: KITTY_PROTOCOL,
                        })?,
                    self.image_height_pixels
                        .ok_or(GraphicsError::InvalidDimensions {
                            protocol: KITTY_PROTOCOL,
                        })?,
                    &decoded_payload_bytes,
                )?,
                None,
            ),
            KittyFormat::Rgba => (
                decode_raw_rgba(
                    KITTY_PROTOCOL,
                    self.image_width_pixels
                        .ok_or(GraphicsError::InvalidDimensions {
                            protocol: KITTY_PROTOCOL,
                        })?,
                    self.image_height_pixels
                        .ok_or(GraphicsError::InvalidDimensions {
                            protocol: KITTY_PROTOCOL,
                        })?,
                    &decoded_payload_bytes,
                )?,
                None,
            ),
            KittyFormat::Png => {
                if !decoded_payload_bytes.starts_with(PNG_SIGNATURE_BYTES) {
                    return Err(GraphicsError::DecodeFailure {
                        protocol: KITTY_PROTOCOL,
                    });
                }
                (decode_png(KITTY_PROTOCOL, &decoded_payload_bytes)?, None)
            }
        };
        Ok(DecodedGraphics {
            is_query: self.is_query,
            protocol: KITTY_PROTOCOL,
            image: decoded_image,
            animation,
            action: self.action,
            display: self.image_display,
        })
    }
}

enum TransferData {
    EncodedPayloadBytes(Vec<u8>),
    DecodedPayloadBytes(Vec<u8>),
}

struct TransferRequest<'a> {
    transfer_medium: u8,
    source_payload_bytes: &'a [u8],
    source_byte_offset: Option<usize>,
    source_byte_count: Option<usize>,
    is_compressed: bool,
    is_final_chunk: bool,
    media_format: Option<KittyFormat>,
    image_width_pixels: Option<u32>,
    image_height_pixels: Option<u32>,
}

fn load_transfer_payload(
    transfer_request: TransferRequest<'_>,
) -> Result<TransferData, GraphicsError> {
    let TransferRequest {
        transfer_medium,
        source_payload_bytes,
        source_byte_offset,
        source_byte_count,
        is_compressed,
        is_final_chunk,
        media_format,
        image_width_pixels,
        image_height_pixels,
    } = transfer_request;
    if !is_final_chunk {
        if transfer_medium != b'd' || source_byte_offset.is_some() {
            return Err(GraphicsError::InvalidCommand {
                protocol: KITTY_PROTOCOL,
            });
        }
        return Ok(TransferData::EncodedPayloadBytes(
            source_payload_bytes.to_vec(),
        ));
    }

    let transfer_source_bytes = match transfer_medium {
        b'd' => decode_base64(KITTY_PROTOCOL, source_payload_bytes)?,
        b'f' | b't' | b's' => load_external_transfer_payload(
            transfer_medium,
            source_payload_bytes,
            source_byte_offset,
            source_byte_count,
        )?,
        unsupported_transfer_medium => {
            return Err(GraphicsError::UnsupportedAction {
                protocol: KITTY_PROTOCOL,
                action: format!("transfer medium {}", unsupported_transfer_medium as char),
            })
        }
    };
    let decoded_payload_bytes = if transfer_medium == b's' && source_byte_count.is_none() {
        exact_shared_memory_payload(
            &transfer_source_bytes,
            media_format.unwrap_or(KittyFormat::Rgba),
            image_width_pixels,
            image_height_pixels,
            is_compressed,
        )?
    } else if is_compressed {
        decompress_bounded(KITTY_PROTOCOL, &transfer_source_bytes)?
    } else {
        transfer_source_bytes
    };
    if transfer_medium == b'd' && is_compressed && media_format == Some(KittyFormat::Png) {
        if let Some(declared_payload_byte_count) = source_byte_count {
            if declared_payload_byte_count != decoded_payload_bytes.len() {
                return Err(GraphicsError::DeclaredSizeMismatch {
                    protocol: KITTY_PROTOCOL,
                    expected_byte_count: declared_payload_byte_count,
                    actual_byte_count: decoded_payload_bytes.len(),
                });
            }
        }
    }
    Ok(TransferData::DecodedPayloadBytes(decoded_payload_bytes))
}

fn exact_shared_memory_payload(
    shared_memory_bytes: &[u8],
    media_format: KittyFormat,
    image_width_pixels: Option<u32>,
    image_height_pixels: Option<u32>,
    is_compressed: bool,
) -> Result<Vec<u8>, GraphicsError> {
    if is_compressed {
        return decompress_bounded_prefix(KITTY_PROTOCOL, shared_memory_bytes)
            .map(|(decoded_payload_bytes, _)| decoded_payload_bytes);
    }
    let payload_byte_count = match media_format {
        KittyFormat::Rgb | KittyFormat::Rgba => {
            let image_width_pixels =
                image_width_pixels.ok_or(GraphicsError::InvalidDimensions {
                    protocol: KITTY_PROTOCOL,
                })?;
            let image_height_pixels =
                image_height_pixels.ok_or(GraphicsError::InvalidDimensions {
                    protocol: KITTY_PROTOCOL,
                })?;
            let channel_count = if media_format == KittyFormat::Rgb {
                3
            } else {
                4
            };
            let image_width_pixels =
                usize::try_from(image_width_pixels).map_err(|_| GraphicsError::ImageTooLarge {
                    protocol: KITTY_PROTOCOL,
                })?;
            let image_height_pixels =
                usize::try_from(image_height_pixels).map_err(|_| GraphicsError::ImageTooLarge {
                    protocol: KITTY_PROTOCOL,
                })?;
            image_width_pixels
                .checked_mul(image_height_pixels)
                .and_then(|image_pixel_count| image_pixel_count.checked_mul(channel_count))
                .ok_or(GraphicsError::ImageTooLarge {
                    protocol: KITTY_PROTOCOL,
                })?
        }
        KittyFormat::Png => compute_png_stream_byte_count(shared_memory_bytes)?,
    };
    if payload_byte_count > MAX_GRAPHICS_TRANSFER_BYTE_COUNT {
        return Err(GraphicsError::TransferTooLarge {
            protocol: KITTY_PROTOCOL,
        });
    }
    shared_memory_bytes
        .get(..payload_byte_count)
        .map(<[u8]>::to_vec)
        .ok_or(GraphicsError::DecodeFailure {
            protocol: KITTY_PROTOCOL,
        })
}

fn compute_png_stream_byte_count(png_bytes: &[u8]) -> Result<usize, GraphicsError> {
    if !png_bytes.starts_with(PNG_SIGNATURE_BYTES) {
        return Err(GraphicsError::DecodeFailure {
            protocol: KITTY_PROTOCOL,
        });
    }
    let mut png_byte_offset = PNG_SIGNATURE_BYTES.len();
    loop {
        let png_chunk_header = png_bytes
            .get(png_byte_offset..png_byte_offset.saturating_add(8))
            .ok_or(GraphicsError::DecodeFailure {
                protocol: KITTY_PROTOCOL,
            })?;
        let png_chunk_byte_count = usize::try_from(u32::from_be_bytes(
            png_chunk_header[..4]
                .try_into()
                .map_err(|_| GraphicsError::DecodeFailure {
                    protocol: KITTY_PROTOCOL,
                })?,
        ))
        .map_err(|_| GraphicsError::DecodeFailure {
            protocol: KITTY_PROTOCOL,
        })?;
        let png_chunk_end_byte_offset = png_byte_offset
            .checked_add(12)
            .and_then(|png_chunk_start_byte_offset| {
                png_chunk_start_byte_offset.checked_add(png_chunk_byte_count)
            })
            .ok_or(GraphicsError::ImageTooLarge {
                protocol: KITTY_PROTOCOL,
            })?;
        if png_chunk_end_byte_offset > png_bytes.len() {
            return Err(GraphicsError::DecodeFailure {
                protocol: KITTY_PROTOCOL,
            });
        }
        if &png_chunk_header[4..8] == b"IEND" {
            return Ok(png_chunk_end_byte_offset);
        }
        png_byte_offset = png_chunk_end_byte_offset;
    }
}

fn validate_transfer_controls(
    kitty_control: &KittyControl,
    transfer_medium: u8,
) -> Result<(), GraphicsError> {
    if transfer_medium != b'd' && kitty_control.has_more_chunks {
        return Err(GraphicsError::InvalidCommand {
            protocol: KITTY_PROTOCOL,
        });
    }
    if transfer_medium == b'd' && kitty_control.source_byte_offset.is_some() {
        return Err(GraphicsError::InvalidCommand {
            protocol: KITTY_PROTOCOL,
        });
    }
    if kitty_control.media_format == Some(KittyFormat::Png)
        && kitty_control.is_compressed == Some(true)
        && kitty_control.source_byte_count.is_none()
        && transfer_medium == b'd'
    {
        return Err(GraphicsError::InvalidHeader {
            protocol: KITTY_PROTOCOL,
        });
    }
    Ok(())
}

fn prepare_animation_payload(
    control_header_bytes: &[u8],
    encoded_payload_bytes: &[u8],
    is_final_chunk: bool,
) -> Result<Vec<u8>, GraphicsError> {
    let kitty_control = parse_animation_control(control_header_bytes)?;
    let transfer_medium = kitty_control.transfer_medium.unwrap_or(b'd');
    validate_transfer_controls(&kitty_control, transfer_medium)?;
    let transfer_payload = load_transfer_payload(TransferRequest {
        transfer_medium,
        source_payload_bytes: encoded_payload_bytes,
        source_byte_offset: kitty_control.source_byte_offset,
        source_byte_count: kitty_control.source_byte_count,
        is_compressed: kitty_control.is_compressed == Some(true),
        is_final_chunk,
        media_format: kitty_control.media_format,
        image_width_pixels: kitty_control.image_width_pixels,
        image_height_pixels: kitty_control.image_height_pixels,
    })?;
    match transfer_payload {
        TransferData::DecodedPayloadBytes(decoded_payload_bytes) => {
            Ok(STANDARD.encode(decoded_payload_bytes).into_bytes())
        }
        TransferData::EncodedPayloadBytes(_) => Err(GraphicsError::InvalidBase64 {
            protocol: KITTY_PROTOCOL,
        }),
    }
}

fn parse_animation_control(control_header_bytes: &[u8]) -> Result<KittyControl, GraphicsError> {
    let control_body_bytes =
        control_header_bytes
            .strip_prefix(b"G")
            .ok_or(GraphicsError::InvalidHeader {
                protocol: KITTY_PROTOCOL,
            })?;
    let mut normalized_control_bytes = Vec::new();
    for control_field in control_body_bytes.split(|byte| *byte == b',') {
        if control_field.starts_with(b"a=") {
            normalized_control_bytes.extend_from_slice(b"a=t");
        } else {
            normalized_control_bytes.extend_from_slice(control_field);
        }
        normalized_control_bytes.push(b',');
    }
    if normalized_control_bytes.last() == Some(&b',') {
        normalized_control_bytes.pop();
    }
    parse_kitty_control(&normalized_control_bytes)
}

fn load_external_transfer_payload(
    transfer_medium: u8,
    encoded_source_name_bytes: &[u8],
    source_byte_offset: Option<usize>,
    source_byte_count: Option<usize>,
) -> Result<Vec<u8>, GraphicsError> {
    let source_name_bytes = decode_base64(KITTY_PROTOCOL, encoded_source_name_bytes)?;
    if source_name_bytes.is_empty() || source_name_bytes.len() > MAX_GRAPHICS_TRANSFER_BYTE_COUNT {
        return Err(GraphicsError::InvalidHeader {
            protocol: KITTY_PROTOCOL,
        });
    }
    let source_byte_offset = source_byte_offset.unwrap_or(0);
    match transfer_medium {
        b'f' => load_regular_file_payload(
            &source_name_bytes,
            source_byte_offset,
            source_byte_count,
            false,
        ),
        b't' => load_regular_file_payload(
            &source_name_bytes,
            source_byte_offset,
            source_byte_count,
            true,
        ),
        b's' => {
            load_shared_memory_payload(&source_name_bytes, source_byte_offset, source_byte_count)
        }
        _ => Err(GraphicsError::UnsupportedAction {
            protocol: KITTY_PROTOCOL,
            action: format!("transfer medium {}", transfer_medium as char),
        }),
    }
}

fn load_regular_file_payload(
    source_name_bytes: &[u8],
    source_byte_offset: usize,
    source_byte_count: Option<usize>,
    should_delete_source: bool,
) -> Result<Vec<u8>, GraphicsError> {
    let supplied_source_path = parse_source_path_bytes(source_name_bytes)?;
    let source_path = if should_delete_source {
        find_disposable_file_path(&supplied_source_path).ok_or_else(build_external_source_error)?
    } else {
        supplied_source_path
    };
    let mut source_file = open_regular_source_file(&source_path)?;
    let source_read_result = source_file
        .metadata()
        .map_err(|_| build_external_source_error())
        .and_then(|source_metadata| {
            if source_metadata.file_type().is_file() {
                load_file_byte_range(&mut source_file, source_byte_offset, source_byte_count)
            } else {
                Err(build_external_source_error())
            }
        });
    drop(source_file);
    if should_delete_source {
        let is_source_removed = fs::remove_file(&source_path).is_ok();
        if !is_source_removed {
            return Err(build_external_source_error());
        }
    }
    source_read_result
}

#[cfg(unix)]
fn open_regular_source_file(source_path: &Path) -> Result<File, GraphicsError> {
    use std::os::unix::fs::OpenOptionsExt;

    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(source_path)
        .map_err(|_| build_external_source_error())
}

#[cfg(not(unix))]
fn open_regular_source_file(source_path: &Path) -> Result<File, GraphicsError> {
    File::open(source_path).map_err(|_| build_external_source_error())
}

fn load_file_byte_range(
    source_file: &mut File,
    source_byte_offset: usize,
    requested_byte_count: Option<usize>,
) -> Result<Vec<u8>, GraphicsError> {
    let source_file_byte_count = source_file
        .metadata()
        .map_err(|_| build_external_source_error())?
        .len();
    let source_byte_offset =
        u64::try_from(source_byte_offset).map_err(|_| build_external_source_error())?;
    if source_byte_offset > source_file_byte_count {
        return Err(build_external_source_error());
    }
    let available_byte_count = source_file_byte_count - source_byte_offset;
    let requested_byte_count = match requested_byte_count {
        Some(requested_byte_count) => {
            if u64::try_from(requested_byte_count).map_err(|_| build_external_source_error())?
                > available_byte_count
            {
                return Err(build_external_source_error());
            }
            requested_byte_count
        }
        None => {
            usize::try_from(available_byte_count).map_err(|_| build_external_source_too_large())?
        }
    };
    if requested_byte_count > MAX_GRAPHICS_TRANSFER_BYTE_COUNT {
        return Err(build_external_source_too_large());
    }
    source_file
        .seek(SeekFrom::Start(source_byte_offset))
        .map_err(|_| build_external_source_error())?;
    let mut source_payload_bytes = Vec::new();
    source_payload_bytes
        .try_reserve_exact(requested_byte_count)
        .map_err(|_| build_external_source_too_large())?;
    let mut remaining_byte_count = requested_byte_count;
    let mut source_read_buffer = [0u8; 8192];
    while remaining_byte_count != 0 {
        let read_request_byte_count = remaining_byte_count.min(source_read_buffer.len());
        let read_byte_count = source_file
            .read(&mut source_read_buffer[..read_request_byte_count])
            .map_err(|_| build_external_source_error())?;
        if read_byte_count == 0 {
            return Err(build_external_source_error());
        }
        source_payload_bytes.extend_from_slice(&source_read_buffer[..read_byte_count]);
        remaining_byte_count -= read_byte_count;
    }
    Ok(source_payload_bytes)
}

fn find_disposable_file_path(source_path: &Path) -> Option<PathBuf> {
    let canonical_source_path = fs::canonicalize(source_path).ok()?;
    let canonical_path_text = canonical_source_path.to_string_lossy().to_ascii_lowercase();
    if !canonical_path_text.contains("tty-graphics-protocol") {
        return None;
    }
    let mut disposable_roots = Vec::new();
    if let Ok(disposable_root) = fs::canonicalize(std::env::temp_dir()) {
        disposable_roots.push(disposable_root);
    }
    #[cfg(unix)]
    {
        for candidate_root in [Path::new("/tmp"), Path::new("/dev/shm")] {
            if let Ok(disposable_root) = fs::canonicalize(candidate_root) {
                disposable_roots.push(disposable_root);
            }
        }
    }
    disposable_roots
        .iter()
        .any(|disposable_root| canonical_source_path.starts_with(disposable_root))
        .then_some(canonical_source_path)
}

fn parse_source_path_bytes(source_path_bytes: &[u8]) -> Result<PathBuf, GraphicsError> {
    #[cfg(unix)]
    {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        if source_path_bytes.contains(&0) {
            return Err(build_external_source_error());
        }
        Ok(PathBuf::from(OsString::from_vec(
            source_path_bytes.to_vec(),
        )))
    }
    #[cfg(not(unix))]
    {
        String::from_utf8(source_path_bytes.to_vec())
            .map(PathBuf::from)
            .map_err(|_| build_external_source_error())
    }
}

#[cfg(unix)]
fn load_shared_memory_payload(
    shared_memory_name_bytes: &[u8],
    source_byte_offset: usize,
    source_byte_count: Option<usize>,
) -> Result<Vec<u8>, GraphicsError> {
    let shared_memory_name =
        CString::new(shared_memory_name_bytes).map_err(|_| build_external_source_error())?;
    let shared_memory_descriptor =
        unsafe { libc::shm_open(shared_memory_name.as_ptr(), libc::O_RDONLY, 0) };
    if shared_memory_descriptor < 0 {
        return Err(build_external_source_error());
    }
    let mut shared_memory_file = unsafe { File::from_raw_fd(shared_memory_descriptor) };
    let source_read_result = load_file_byte_range(
        &mut shared_memory_file,
        source_byte_offset,
        source_byte_count,
    );
    drop(shared_memory_file);
    let is_unlinked = unsafe { libc::shm_unlink(shared_memory_name.as_ptr()) } == 0;
    if !is_unlinked && source_read_result.is_ok() {
        return Err(build_external_source_error());
    }
    source_read_result
}

#[cfg(windows)]
fn load_shared_memory_payload(
    shared_memory_name_bytes: &[u8],
    source_byte_offset: usize,
    source_byte_count: Option<usize>,
) -> Result<Vec<u8>, GraphicsError> {
    use std::ffi::c_void;
    use std::mem::size_of;
    use std::ptr;
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Memory::{
        MapViewOfFile, OpenFileMappingW, UnmapViewOfFile, VirtualQuery, FILE_MAP_READ,
        MEMORY_BASIC_INFORMATION,
    };
    use windows_sys::Win32::System::SystemInformation::{GetSystemInfo, SYSTEM_INFO};

    let shared_memory_name = String::from_utf8(shared_memory_name_bytes.to_vec())
        .map_err(|_| build_external_source_error())?;
    let mut wide_name_units = shared_memory_name.encode_utf16().collect::<Vec<_>>();
    wide_name_units.push(0);
    let shared_memory_mapping =
        unsafe { OpenFileMappingW(FILE_MAP_READ, 0, wide_name_units.as_ptr()) };
    if shared_memory_mapping.is_null() {
        return Err(build_external_source_error());
    }
    let shared_memory_mapping_result = (|| {
        let source_byte_offset_u64 =
            u64::try_from(source_byte_offset).map_err(|_| build_external_source_error())?;
        let mut windows_system_info = SYSTEM_INFO::default();
        unsafe { GetSystemInfo(&mut windows_system_info) };
        let allocation_granularity_bytes = u64::from(windows_system_info.dwAllocationGranularity);
        if allocation_granularity_bytes == 0 {
            return Err(build_external_source_error());
        }
        let aligned_source_byte_offset =
            source_byte_offset_u64 - source_byte_offset_u64 % allocation_granularity_bytes;
        let mapped_view_byte_offset =
            usize::try_from(source_byte_offset_u64 - aligned_source_byte_offset)
                .map_err(|_| build_external_source_error())?;
        if source_byte_count.is_some_and(|requested_byte_count| {
            requested_byte_count > MAX_GRAPHICS_TRANSFER_BYTE_COUNT
        }) {
            return Err(build_external_source_too_large());
        }
        let requested_view_byte_count = match source_byte_count {
            Some(requested_byte_count) => Some(
                mapped_view_byte_offset
                    .checked_add(requested_byte_count)
                    .ok_or_else(build_external_source_error)?,
            ),
            None => None,
        };
        let mapped_view = unsafe {
            MapViewOfFile(
                shared_memory_mapping,
                FILE_MAP_READ,
                u32::try_from(aligned_source_byte_offset >> 32)
                    .map_err(|_| build_external_source_error())?,
                u32::try_from(aligned_source_byte_offset)
                    .map_err(|_| build_external_source_error())?,
                requested_view_byte_count.unwrap_or(0),
            )
        };
        if mapped_view.Value.is_null() {
            return Err(build_external_source_error());
        }
        let mapped_view_result = (|| {
            let payload_byte_count = if let Some(requested_byte_count) = source_byte_count {
                requested_byte_count
            } else {
                let mut memory_information = MEMORY_BASIC_INFORMATION::default();
                let queried_byte_count = unsafe {
                    VirtualQuery(
                        mapped_view.Value as *const c_void,
                        &mut memory_information,
                        size_of::<MEMORY_BASIC_INFORMATION>(),
                    )
                };
                if queried_byte_count == 0
                    || mapped_view_byte_offset > memory_information.RegionSize
                {
                    return Err(build_external_source_error());
                }
                (memory_information.RegionSize - mapped_view_byte_offset)
                    .min(MAX_GRAPHICS_TRANSFER_BYTE_COUNT)
            };
            let mut shared_memory_bytes = Vec::new();
            shared_memory_bytes
                .try_reserve_exact(payload_byte_count)
                .map_err(|_| build_external_source_too_large())?;
            shared_memory_bytes.resize(payload_byte_count, 0);
            let source_pointer =
                unsafe { (mapped_view.Value as *const u8).add(mapped_view_byte_offset) };
            unsafe {
                ptr::copy_nonoverlapping(
                    source_pointer,
                    shared_memory_bytes.as_mut_ptr(),
                    payload_byte_count,
                )
            };
            Ok(shared_memory_bytes)
        })();
        unsafe { UnmapViewOfFile(mapped_view) };
        mapped_view_result
    })();
    unsafe { CloseHandle(shared_memory_mapping) };
    shared_memory_mapping_result
}

#[cfg(not(any(unix, windows)))]
fn load_shared_memory_payload(
    _shared_memory_name_bytes: &[u8],
    _source_byte_offset: usize,
    _source_byte_count: Option<usize>,
) -> Result<Vec<u8>, GraphicsError> {
    Err(build_external_source_error())
}

fn build_external_source_error() -> GraphicsError {
    GraphicsError::DecodeFailure {
        protocol: KITTY_PROTOCOL,
    }
}

fn build_external_source_too_large() -> GraphicsError {
    GraphicsError::TransferTooLarge {
        protocol: KITTY_PROTOCOL,
    }
}

impl KittyAnimationTransfer {
    /// Return display metadata from the first animation-frame chunk.
    #[must_use]
    pub fn get_image_display(&self) -> ImageDisplay {
        commands::parse_reply_display(&self.control_header_bytes)
    }

    /// Validate and append a continuation chunk.
    pub fn accept_continuation_chunk(
        mut self,
        chunk: KittyAnimationChunk,
    ) -> Result<KittyAnimationTransferOutcome, GraphicsError> {
        if !chunk.is_continuation() {
            return Err(GraphicsError::InvalidHeader {
                protocol: KITTY_PROTOCOL,
            });
        }
        let control_header_bytes = chunk.get_control_header_bytes();
        let control_body_bytes =
            control_header_bytes
                .strip_prefix(b"G")
                .ok_or(GraphicsError::InvalidHeader {
                    protocol: KITTY_PROTOCOL,
                })?;
        if !control_body_bytes
            .split(|control_byte| *control_byte == b',')
            .filter(|control_field| !control_field.is_empty())
            .all(|control_field| {
                control_field.starts_with(b"a=f")
                    || control_field.starts_with(b"m=")
                    || control_field.starts_with(b"q=")
            })
        {
            return Err(GraphicsError::InvalidHeader {
                protocol: KITTY_PROTOCOL,
            });
        }
        append_bounded_bytes(
            &mut self.encoded_payload_bytes,
            chunk.get_encoded_payload_bytes(),
        )?;
        if chunk.has_more_chunks() {
            Ok(KittyAnimationTransferOutcome::Pending(self))
        } else {
            let prepared_payload_bytes = prepare_animation_payload(
                &self.control_header_bytes,
                &self.encoded_payload_bytes,
                true,
            )?;
            let control_body_bytes = self.control_header_bytes.strip_prefix(b"G").ok_or(
                GraphicsError::InvalidHeader {
                    protocol: KITTY_PROTOCOL,
                },
            )?;
            let command = commands::parse_animation_command_fields(
                control_body_bytes,
                &prepared_payload_bytes,
                KittyCommandKind::AnimationFrame,
                true,
            )?;
            Ok(KittyAnimationTransferOutcome::Complete(Box::new(command)))
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum KittyFormat {
    Rgb,
    Rgba,
    Png,
}

#[derive(Debug)]
pub(super) struct KittyControl {
    pub(super) is_query: bool,
    pub(super) image_id: Option<u32>,
    pub(super) action: ImageAction,
    pub(super) transfer_medium: Option<u8>,
    pub(super) media_format: Option<KittyFormat>,
    pub(super) image_width_pixels: Option<u32>,
    pub(super) image_height_pixels: Option<u32>,
    pub(super) has_more_chunks: bool,
    pub(super) has_more_chunks_parameter: bool,
    pub(super) is_compressed: Option<bool>,
    pub(super) source_byte_count: Option<usize>,
    pub(super) source_byte_offset: Option<usize>,
    pub(super) image_display: ImageDisplay,
    pub(super) supports_continuation: bool,
}

impl Default for KittyControl {
    fn default() -> Self {
        Self {
            is_query: false,
            image_id: None,
            action: ImageAction::Transmit,
            transfer_medium: None,
            media_format: None,
            image_width_pixels: None,
            image_height_pixels: None,
            has_more_chunks: false,
            has_more_chunks_parameter: false,
            is_compressed: None,
            source_byte_count: None,
            source_byte_offset: None,
            image_display: ImageDisplay::default(),
            supports_continuation: true,
        }
    }
}

pub(super) fn parse_kitty_control(control_bytes: &[u8]) -> Result<KittyControl, GraphicsError> {
    let mut kitty_control = KittyControl::default();
    let mut seen_control_keys = [false; 256];
    for control_field_bytes in control_bytes.split(|byte| *byte == b',') {
        if control_field_bytes.is_empty() {
            continue;
        }
        let (control_key_bytes, parameter_value) =
            split_bytes_at_delimiter(control_field_bytes, b'=').ok_or(
                GraphicsError::InvalidHeader {
                    protocol: KITTY_PROTOCOL,
                },
            )?;
        if control_key_bytes.len() != 1 || parameter_value.is_empty() {
            return Err(GraphicsError::InvalidHeader {
                protocol: KITTY_PROTOCOL,
            });
        }
        let control_key = control_key_bytes[0];
        let control_key_slot = match control_key {
            b'a' | b'C' | b'c' | b'd' | b'f' | b'h' | b'H' | b'i' | b'I' | b'm' | b'N' | b'o'
            | b'O' | b'p' | b'P' | b'q' | b'Q' | b'r' | b's' | b'S' | b't' | b'U' | b'v' | b'V'
            | b'w' | b'x' | b'X' | b'y' | b'Y' | b'z' => usize::from(control_key),
            _ => {
                return Err(GraphicsError::InvalidHeader {
                    protocol: KITTY_PROTOCOL,
                })
            }
        };
        if seen_control_keys[control_key_slot] {
            return Err(GraphicsError::InvalidHeader {
                protocol: KITTY_PROTOCOL,
            });
        }
        seen_control_keys[control_key_slot] = true;
        if !matches!(control_key, b'm' | b'q') {
            kitty_control.supports_continuation = false;
        }
        match control_key {
            b'a' => {
                let action_name = parse_ascii_text(parameter_value)?;
                kitty_control.action = match parameter_value {
                    b"t" => ImageAction::Transmit,
                    b"T" => ImageAction::TransmitAndDisplay,
                    b"q" => {
                        kitty_control.is_query = true;
                        ImageAction::Transmit
                    }
                    _ => {
                        return Err(GraphicsError::UnsupportedAction {
                            protocol: KITTY_PROTOCOL,
                            action: action_name,
                        })
                    }
                };
            }
            b'f' => {
                kitty_control.media_format = Some(match parse_decimal_u32(parameter_value)? {
                    24 => KittyFormat::Rgb,
                    32 => KittyFormat::Rgba,
                    100 => KittyFormat::Png,
                    _ => {
                        return Err(GraphicsError::UnsupportedMedia {
                            protocol: KITTY_PROTOCOL,
                            media_format: String::from_utf8_lossy(parameter_value).into_owned(),
                        })
                    }
                });
            }
            b'h' => {
                let requested_pixel_height = parse_decimal_u32(parameter_value)?;
                if requested_pixel_height == 0 {
                    return Err(GraphicsError::InvalidDimensions {
                        protocol: KITTY_PROTOCOL,
                    });
                }
                kitty_control.image_display.requested_height =
                    Some(koshi_image::ImageDimension::Pixels(requested_pixel_height));
            }
            b'i' => {
                if kitty_control.image_display.image_number.is_some() {
                    return Err(GraphicsError::InvalidHeader {
                        protocol: KITTY_PROTOCOL,
                    });
                }
                let image_id = parse_decimal_u32(parameter_value)?;
                if image_id == 0 {
                    return Err(GraphicsError::InvalidCommand {
                        protocol: KITTY_PROTOCOL,
                    });
                }
                kitty_control.image_id = Some(image_id);
                kitty_control.image_display.image_id = Some(image_id);
            }
            b'I' => {
                if kitty_control.image_id.is_some() {
                    return Err(GraphicsError::InvalidHeader {
                        protocol: KITTY_PROTOCOL,
                    });
                }
                let image_number = parse_decimal_u32(parameter_value)?;
                if image_number == 0 {
                    return Err(GraphicsError::InvalidCommand {
                        protocol: KITTY_PROTOCOL,
                    });
                }
                kitty_control.image_display.image_number = Some(image_number);
            }
            b'm' => match parameter_value {
                b"0" => {
                    kitty_control.has_more_chunks = false;
                    kitty_control.has_more_chunks_parameter = true;
                }
                b"1" => {
                    kitty_control.has_more_chunks = true;
                    kitty_control.has_more_chunks_parameter = true;
                }
                _ => {
                    return Err(GraphicsError::InvalidCommand {
                        protocol: KITTY_PROTOCOL,
                    })
                }
            },
            b'o' => {
                kitty_control.is_compressed = Some(match parameter_value {
                    b"z" => true,
                    _ => {
                        return Err(GraphicsError::UnsupportedMedia {
                            protocol: KITTY_PROTOCOL,
                            media_format: parse_ascii_text(parameter_value)?,
                        })
                    }
                });
            }
            b's' => kitty_control.image_width_pixels = Some(parse_decimal_u32(parameter_value)?),
            b'v' => kitty_control.image_height_pixels = Some(parse_decimal_u32(parameter_value)?),
            b'p' => {
                kitty_control.image_display.placement_id =
                    Some(parse_decimal_u32(parameter_value)?);
            }
            b'S' => kitty_control.source_byte_count = Some(parse_decimal_usize(parameter_value)?),
            b'O' => kitty_control.source_byte_offset = Some(parse_decimal_usize(parameter_value)?),
            b't' => {
                if parameter_value.len() != 1 {
                    return Err(GraphicsError::InvalidCommand {
                        protocol: KITTY_PROTOCOL,
                    });
                }
                kitty_control.transfer_medium = Some(parameter_value[0]);
            }
            b'c' => {
                kitty_control.image_display.requested_column_count =
                    Some(parse_positive_decimal_u32(parameter_value)?);
            }
            b'r' => {
                kitty_control.image_display.requested_row_count =
                    Some(parse_positive_decimal_u32(parameter_value)?);
            }
            b'w' => {
                kitty_control.image_display.requested_width =
                    Some(koshi_image::ImageDimension::Pixels(
                        parse_positive_decimal_u32(parameter_value)?,
                    ));
            }
            b'x' => {
                kitty_control.image_display.source_pixel_offset_x =
                    Some(parse_decimal_u32(parameter_value)?);
            }
            b'y' => {
                kitty_control.image_display.source_pixel_offset_y =
                    Some(parse_decimal_u32(parameter_value)?);
            }
            b'X' => {
                kitty_control.image_display.cell_pixel_offset_x =
                    Some(parse_decimal_u32(parameter_value)?);
            }
            b'Y' => {
                kitty_control.image_display.cell_pixel_offset_y =
                    Some(parse_decimal_u32(parameter_value)?);
            }
            b'C' => {
                kitty_control.image_display.should_move_cursor =
                    match parse_decimal_u32(parameter_value)? {
                        0 => true,
                        1 => false,
                        _ => {
                            return Err(GraphicsError::InvalidCommand {
                                protocol: KITTY_PROTOCOL,
                            })
                        }
                    };
            }
            b'N' => {
                kitty_control.image_display.usage_hints = parse_decimal_u32(parameter_value)?;
            }
            b'U' => {
                kitty_control.image_display.is_unicode_placeholder =
                    match parse_decimal_u32(parameter_value)? {
                        0 => false,
                        1 => true,
                        _ => {
                            return Err(GraphicsError::InvalidCommand {
                                protocol: KITTY_PROTOCOL,
                            })
                        }
                    };
            }
            b'P' => {
                kitty_control.image_display.relative_image_id =
                    Some(parse_positive_decimal_u32(parameter_value)?);
            }
            b'Q' => {
                kitty_control.image_display.relative_placement_id =
                    Some(parse_positive_decimal_u32(parameter_value)?);
            }
            b'H' => {
                kitty_control.image_display.relative_column_offset =
                    parse_decimal_i32(parameter_value)?;
            }
            b'V' => {
                kitty_control.image_display.relative_row_offset =
                    parse_decimal_i32(parameter_value)?;
            }
            b'z' => {
                kitty_control.image_display.z_index = parse_decimal_i32(parameter_value)?;
            }
            b'q' => {
                let response_suppression_level = parse_decimal_u32(parameter_value)?;
                kitty_control.image_display.response_suppression_level =
                    response_suppression_level.min(2) as u8;
                if response_suppression_level > 2 {
                    return Err(GraphicsError::InvalidCommand {
                        protocol: KITTY_PROTOCOL,
                    });
                }
            }
            b'd' => {
                return Err(GraphicsError::UnsupportedAction {
                    protocol: KITTY_PROTOCOL,
                    action: format!("control {}", control_key as char),
                });
            }
            _ => unreachable!(),
        }
    }
    Ok(kitty_control)
}

fn validate_kitty_continuation(
    transfer: &KittyTransfer,
    chunk: &KittyChunk,
) -> Result<(), GraphicsError> {
    if !chunk.has_more_chunks_parameter || !chunk.supports_continuation {
        return Err(GraphicsError::InvalidHeader {
            protocol: KITTY_PROTOCOL,
        });
    }
    if let Some(image_id) = chunk.image_id {
        if transfer.image_id != Some(image_id) {
            return Err(GraphicsError::InvalidHeader {
                protocol: KITTY_PROTOCOL,
            });
        }
    }
    if let Some(media_format) = chunk.media_format {
        if media_format != transfer.media_format {
            return Err(GraphicsError::InvalidHeader {
                protocol: KITTY_PROTOCOL,
            });
        }
    }
    if let Some(image_width_pixels) = chunk.image_width_pixels {
        if transfer.image_width_pixels != Some(image_width_pixels) {
            return Err(GraphicsError::InvalidDimensions {
                protocol: KITTY_PROTOCOL,
            });
        }
    }
    if let Some(image_height_pixels) = chunk.image_height_pixels {
        if transfer.image_height_pixels != Some(image_height_pixels) {
            return Err(GraphicsError::InvalidDimensions {
                protocol: KITTY_PROTOCOL,
            });
        }
    }
    if let Some(is_compressed) = chunk.is_compressed {
        if is_compressed != transfer.is_compressed {
            return Err(GraphicsError::InvalidCommand {
                protocol: KITTY_PROTOCOL,
            });
        }
    }
    if let Some(declared_payload_byte_count) = chunk.declared_payload_byte_count {
        if transfer.declared_payload_byte_count.is_some()
            && transfer.declared_payload_byte_count != Some(declared_payload_byte_count)
        {
            return Err(GraphicsError::InvalidHeader {
                protocol: KITTY_PROTOCOL,
            });
        }
    }
    Ok(())
}

fn append_bounded_bytes(
    encoded_payload_bytes: &mut Vec<u8>,
    payload_bytes: &[u8],
) -> Result<(), GraphicsError> {
    let encoded_payload_byte_count = encoded_payload_bytes
        .len()
        .checked_add(payload_bytes.len())
        .ok_or(GraphicsError::InvalidDimensions {
            protocol: KITTY_PROTOCOL,
        })?;
    if encoded_payload_byte_count > MAX_GRAPHICS_TRANSFER_BYTE_COUNT {
        return Err(GraphicsError::TransferTooLarge {
            protocol: KITTY_PROTOCOL,
        });
    }
    encoded_payload_bytes.extend_from_slice(payload_bytes);
    Ok(())
}

fn push_bounded_bytes(
    bounded_byte_buffer: &mut Vec<u8>,
    input_byte: u8,
    maximum_byte_count: usize,
) -> Result<(), GraphicsError> {
    if bounded_byte_buffer.len() == maximum_byte_count {
        return Err(GraphicsError::TransferTooLarge {
            protocol: KITTY_PROTOCOL,
        });
    }
    bounded_byte_buffer.push(input_byte);
    Ok(())
}

fn parse_decimal_u32(decimal_bytes: &[u8]) -> Result<u32, GraphicsError> {
    if decimal_bytes.is_empty() || !decimal_bytes.iter().all(u8::is_ascii_digit) {
        return Err(GraphicsError::InvalidCommand {
            protocol: KITTY_PROTOCOL,
        });
    }
    let mut decimal_value = 0u32;
    for &decimal_digit in decimal_bytes {
        decimal_value = decimal_value
            .checked_mul(10)
            .and_then(|decimal_value| decimal_value.checked_add(u32::from(decimal_digit - b'0')))
            .ok_or(GraphicsError::InvalidDimensions {
                protocol: KITTY_PROTOCOL,
            })?;
    }
    Ok(decimal_value)
}

fn parse_decimal_i32(decimal_bytes: &[u8]) -> Result<i32, GraphicsError> {
    if decimal_bytes.is_empty() {
        return Err(GraphicsError::InvalidCommand {
            protocol: KITTY_PROTOCOL,
        });
    }
    let (is_negative, decimal_digits) = match decimal_bytes.first() {
        Some(b'-') => (true, &decimal_bytes[1..]),
        _ => (false, decimal_bytes),
    };
    if decimal_digits.is_empty() || !decimal_digits.iter().all(u8::is_ascii_digit) {
        return Err(GraphicsError::InvalidCommand {
            protocol: KITTY_PROTOCOL,
        });
    }
    let mut decimal_value = 0u32;
    for &decimal_digit in decimal_digits {
        decimal_value = decimal_value
            .checked_mul(10)
            .and_then(|decimal_value| decimal_value.checked_add(u32::from(decimal_digit - b'0')))
            .ok_or(GraphicsError::InvalidCommand {
                protocol: KITTY_PROTOCOL,
            })?;
    }
    if is_negative {
        if decimal_value == 2_147_483_648 {
            Ok(i32::MIN)
        } else {
            i32::try_from(decimal_value)
                .ok()
                .and_then(|decimal_value| decimal_value.checked_neg())
                .ok_or(GraphicsError::InvalidCommand {
                    protocol: KITTY_PROTOCOL,
                })
        }
    } else {
        i32::try_from(decimal_value).map_err(|_| GraphicsError::InvalidCommand {
            protocol: KITTY_PROTOCOL,
        })
    }
}

fn parse_positive_decimal_u32(decimal_bytes: &[u8]) -> Result<u32, GraphicsError> {
    let positive_decimal_value = parse_decimal_u32(decimal_bytes)?;
    if positive_decimal_value == 0 {
        return Err(GraphicsError::InvalidDimensions {
            protocol: KITTY_PROTOCOL,
        });
    }
    Ok(positive_decimal_value)
}

fn parse_decimal_usize(decimal_bytes: &[u8]) -> Result<usize, GraphicsError> {
    let decimal_value = parse_decimal_u32(decimal_bytes)?;
    usize::try_from(decimal_value).map_err(|_| GraphicsError::InvalidDimensions {
        protocol: KITTY_PROTOCOL,
    })
}

fn parse_ascii_text(ascii_bytes: &[u8]) -> Result<String, GraphicsError> {
    if !ascii_bytes.is_ascii() {
        return Err(GraphicsError::InvalidHeader {
            protocol: KITTY_PROTOCOL,
        });
    }
    Ok(String::from_utf8_lossy(ascii_bytes).into_owned())
}

fn split_bytes_at_delimiter(source_bytes: &[u8], delimiter_byte: u8) -> Option<(&[u8], &[u8])> {
    let delimiter_index = source_bytes
        .iter()
        .position(|source_byte| *source_byte == delimiter_byte)?;
    Some((
        &source_bytes[..delimiter_index],
        &source_bytes[delimiter_index + 1..],
    ))
}

#[cfg(test)]
mod tests;
