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
    decode_base64, decode_png, decompress_bounded, decompress_bounded_prefix, raw_rgb, raw_rgba,
    validate_dimensions, DecodedGraphics, GraphicsError, GraphicsProtocol, ImageAction,
    ImageDisplay, MAX_GRAPHICS_CONTROL_BYTES, MAX_GRAPHICS_TRANSFER_BYTES,
};

const KITTY_PROTOCOL: GraphicsProtocol = GraphicsProtocol::Kitty;
const PNG_SIGNATURE: &[u8; 8] = b"\x89PNG\r\n\x1a\n";

mod commands;

pub use commands::{
    reply_display, KittyAnimationChunk, KittyAnimationCommand, KittyCommand, KittyCommandKind,
    KittyDelete,
};

/// Parse one non-multipart Kitty command and acquire its animation-frame data.
pub fn parse_command(header: &[u8], payload: &[u8]) -> Option<Result<KittyCommand, GraphicsError>> {
    let action = header
        .strip_prefix(b"G")?
        .split(|byte| *byte == b',')
        .find_map(|field| field.strip_prefix(b"a="));
    let parsed = commands::parse_command(header, payload)?;
    if action != Some(b"f") {
        return Some(parsed);
    }
    Some(parsed.and_then(|_| {
        let prepared = prepare_animation_payload(header, payload, true)?;
        let body = header
            .strip_prefix(b"G")
            .ok_or(GraphicsError::InvalidHeader {
                protocol: KITTY_PROTOCOL,
            })?;
        commands::parse_animation_command_fields(
            body,
            &prepared,
            KittyCommandKind::AnimationFrame,
            false,
        )
    }))
}

/// The largest base64 payload chunk accepted by the Kitty graphics protocol.
pub const MAX_KITTY_CHUNK_BYTES: usize = 4096;

/// Incremental parser for one Kitty APC graphics string.
#[derive(Clone)]
pub struct KittyParser {
    header: Vec<u8>,
    data: Vec<u8>,
    seen_header: bool,
    ignored: bool,
    escaped: bool,
}

impl KittyParser {
    /// Create an empty Kitty APC parser.
    #[must_use]
    pub fn new() -> Self {
        KittyParser {
            header: Vec::new(),
            data: Vec::new(),
            seen_header: false,
            ignored: false,
            escaped: false,
        }
    }

    /// Feed one byte from the APC body.
    pub fn feed(&mut self, byte: u8) -> Result<(), GraphicsError> {
        if self.ignored {
            return Ok(());
        }
        if !self.seen_header {
            if self.header.is_empty() && byte != b'G' {
                self.ignored = true;
                return Ok(());
            }
            if self.header.is_empty() {
                self.header.push(byte);
                return Ok(());
            }
            if byte == b';' {
                self.seen_header = true;
                return Ok(());
            }
            push_bounded(&mut self.header, byte, MAX_GRAPHICS_CONTROL_BYTES)?;
        } else {
            push_bounded(&mut self.data, byte, MAX_KITTY_CHUNK_BYTES)?;
        }
        Ok(())
    }

    /// Return whether the APC body was identified as a non-graphics command.
    #[must_use]
    pub fn is_ignored(&self) -> bool {
        self.ignored
    }

    /// Return whether the framing parser has received an escape byte.
    #[must_use]
    pub fn is_escaped(&self) -> bool {
        self.escaped
    }

    /// Set the framing parser's escape-byte state.
    pub fn set_escaped(&mut self, escaped: bool) {
        self.escaped = escaped;
    }

    /// Return whether the control-data separator has been received.
    #[must_use]
    pub fn has_header(&self) -> bool {
        self.seen_header
    }

    /// Return the bytes received before the control-data separator.
    #[must_use]
    pub fn header(&self) -> &[u8] {
        &self.header
    }

    /// Return the base64 bytes received after the control-data separator.
    #[must_use]
    pub fn payload(&self) -> &[u8] {
        &self.data
    }

    /// Return the number of payload bytes received in this APC string.
    #[must_use]
    pub fn payload_len(&self) -> usize {
        self.data.len()
    }

    /// Append a validated run of base64 payload bytes.
    pub fn append_payload(&mut self, bytes: &[u8]) -> Result<(), GraphicsError> {
        let new_len =
            self.data
                .len()
                .checked_add(bytes.len())
                .ok_or(GraphicsError::TransferTooLarge {
                    protocol: KITTY_PROTOCOL,
                })?;
        if new_len > MAX_KITTY_CHUNK_BYTES {
            return Err(GraphicsError::TransferTooLarge {
                protocol: KITTY_PROTOCOL,
            });
        }
        self.data.extend_from_slice(bytes);
        Ok(())
    }

    /// Finish parsing the control data and payload as one Kitty chunk.
    pub fn finish(self) -> Result<KittyChunk, GraphicsError> {
        if !self.seen_header || self.header.first().copied() != Some(b'G') {
            return Err(GraphicsError::InvalidHeader {
                protocol: KITTY_PROTOCOL,
            });
        }
        let control = parse_kitty_control(&self.header[1..])?;
        KittyChunk::from_control(control, self.data)
    }

    /// Finish parsing a multipart Kitty animation-frame chunk.
    pub fn finish_animation_chunk(self) -> Result<Option<KittyAnimationChunk>, GraphicsError> {
        if !self.seen_header || self.header.first().copied() != Some(b'G') {
            return Err(GraphicsError::InvalidHeader {
                protocol: KITTY_PROTOCOL,
            });
        }
        commands::parse_animation_transfer_chunk(&self.header, &self.data)
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
    query: bool,
    id: Option<u32>,
    action: ImageAction,
    more: bool,
    more_specified: bool,
    continuation_compatible: bool,
    format: Option<KittyFormat>,
    width: Option<u32>,
    height: Option<u32>,
    display: ImageDisplay,
    medium: u8,
    encoded: Vec<u8>,
    raw_data: bool,
    compression: Option<bool>,
    declared_size: Option<usize>,
}

impl KittyChunk {
    fn from_control(control: KittyControl, data: Vec<u8>) -> Result<Self, GraphicsError> {
        if data.len() > MAX_KITTY_CHUNK_BYTES {
            return Err(GraphicsError::TransferTooLarge {
                protocol: KITTY_PROTOCOL,
            });
        }
        if control.more && (!data.len().is_multiple_of(4) || data.contains(&b'=')) {
            return Err(GraphicsError::InvalidBase64 {
                protocol: KITTY_PROTOCOL,
            });
        }
        let medium = control.medium.unwrap_or(b'd');
        let continuation = control.more_specified && control.continuation_compatible;
        let (encoded, raw_data, compression, declared_size) = if continuation {
            if medium != b'd' || control.source_offset.is_some() {
                return Err(GraphicsError::InvalidCommand {
                    protocol: KITTY_PROTOCOL,
                });
            }
            (data, false, control.compression, control.source_size)
        } else {
            validate_transfer_controls(&control, medium)?;
            let data = acquire_transfer_data(TransferRequest {
                medium,
                payload: &data,
                source_offset: control.source_offset,
                source_size: control.source_size,
                compressed: control.compression == Some(true),
                final_chunk: !control.more,
                format: control.format,
                width: control.width,
                height: control.height,
            })?;
            match data {
                TransferData::Encoded(data) => {
                    (data, false, control.compression, control.source_size)
                }
                TransferData::Ready(data) => (data, true, Some(false), None),
            }
        };
        if matches!(control.format, Some(KittyFormat::Rgb | KittyFormat::Rgba))
            && (control.width == Some(0) || control.height == Some(0))
        {
            return Err(GraphicsError::InvalidDimensions {
                protocol: KITTY_PROTOCOL,
            });
        }
        Ok(KittyChunk {
            query: control.query,
            id: control.id,
            action: control.action,
            more: control.more,
            more_specified: control.more_specified,
            continuation_compatible: control.continuation_compatible,
            format: control.format,
            width: control.width,
            height: control.height,
            display: control.display,
            medium,
            encoded,
            raw_data,
            compression,
            declared_size,
        })
    }

    /// Return whether another chunk is required before decoding.
    #[must_use]
    pub fn more(&self) -> bool {
        self.more
    }
}

/// A validated Kitty transfer that is waiting for more chunks.
#[derive(Debug, Clone)]
pub struct KittyTransfer {
    query: bool,
    id: Option<u32>,
    action: ImageAction,
    format: KittyFormat,
    width: Option<u32>,
    height: Option<u32>,
    display: ImageDisplay,
    medium: u8,
    encoded: Vec<u8>,
    raw_data: bool,
    compression: bool,
    declared_size: Option<usize>,
}

/// The result of starting or extending a Kitty transfer.
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
    header: Vec<u8>,
    encoded: Vec<u8>,
}

/// The result of starting or extending a Kitty animation-frame transfer.
#[derive(Debug)]
pub enum KittyAnimationTransferOutcome {
    /// The transfer remains open and must receive another chunk.
    Pending(KittyAnimationTransfer),
    /// The final chunk produced a validated animation command.
    Complete(Box<KittyCommand>),
}

/// Start a Kitty transfer from its first validated chunk.
pub fn start_transfer(chunk: KittyChunk) -> Result<KittyTransferOutcome, GraphicsError> {
    let more = chunk.more;
    let transfer = KittyTransfer::from_chunk(chunk)?;
    if more {
        Ok(KittyTransferOutcome::Pending(transfer))
    } else {
        Ok(KittyTransferOutcome::Complete(transfer.finish()?))
    }
}

/// Start a multipart Kitty animation-frame transfer.
pub fn start_animation_transfer(
    chunk: KittyAnimationChunk,
) -> Result<KittyAnimationTransferOutcome, GraphicsError> {
    if chunk.continuation() || !chunk.more() {
        return Err(GraphicsError::InvalidHeader {
            protocol: KITTY_PROTOCOL,
        });
    }
    let transfer = KittyAnimationTransfer {
        header: chunk.header().to_vec(),
        encoded: chunk.payload().to_vec(),
    };
    Ok(KittyAnimationTransferOutcome::Pending(transfer))
}

impl KittyTransfer {
    fn from_chunk(chunk: KittyChunk) -> Result<Self, GraphicsError> {
        let format = chunk.format.unwrap_or(KittyFormat::Rgba);
        let width = chunk.width;
        let height = chunk.height;
        if matches!(format, KittyFormat::Rgb | KittyFormat::Rgba)
            && (width.is_none() || height.is_none())
        {
            return Err(GraphicsError::InvalidDimensions {
                protocol: KITTY_PROTOCOL,
            });
        }
        if let (Some(width), Some(height)) = (width, height) {
            validate_dimensions(
                KITTY_PROTOCOL,
                usize::try_from(width).map_err(|_| GraphicsError::ImageTooLarge {
                    protocol: KITTY_PROTOCOL,
                })?,
                usize::try_from(height).map_err(|_| GraphicsError::ImageTooLarge {
                    protocol: KITTY_PROTOCOL,
                })?,
            )?;
        }
        Ok(KittyTransfer {
            query: chunk.query,
            id: chunk.id,
            action: chunk.action,
            format,
            width,
            height,
            display: chunk.display,
            medium: chunk.medium,
            encoded: chunk.encoded,
            raw_data: chunk.raw_data,
            compression: chunk.compression.unwrap_or(false),
            declared_size: chunk.declared_size,
        })
    }

    /// Return display metadata used for a response to this transfer.
    #[must_use]
    pub fn display(&self) -> &ImageDisplay {
        &self.display
    }

    /// Validate and append a continuation chunk.
    pub fn accept_chunk(
        mut self,
        chunk: KittyChunk,
    ) -> Result<KittyTransferOutcome, GraphicsError> {
        validate_kitty_continuation(&self, &chunk)?;
        append_bounded(&mut self.encoded, &chunk.encoded)?;
        if chunk.more {
            Ok(KittyTransferOutcome::Pending(self))
        } else {
            if let Some(expected) = chunk.declared_size {
                if self.declared_size.is_some() && self.declared_size != Some(expected) {
                    return Err(GraphicsError::InvalidHeader {
                        protocol: KITTY_PROTOCOL,
                    });
                }
                self.declared_size = Some(expected);
            }
            Ok(KittyTransferOutcome::Complete(self.finish()?))
        }
    }

    fn finish(self) -> Result<DecodedGraphics, GraphicsError> {
        let bytes = if self.raw_data {
            self.encoded
        } else {
            match acquire_transfer_data(TransferRequest {
                medium: self.medium,
                payload: &self.encoded,
                source_offset: None,
                source_size: self.declared_size,
                compressed: self.compression,
                final_chunk: true,
                format: Some(self.format),
                width: self.width,
                height: self.height,
            })? {
                TransferData::Ready(data) => data,
                TransferData::Encoded(_) => {
                    return Err(GraphicsError::InvalidBase64 {
                        protocol: KITTY_PROTOCOL,
                    })
                }
            }
        };
        let (image, animation) = match self.format {
            KittyFormat::Rgb => (
                raw_rgb(
                    KITTY_PROTOCOL,
                    self.width.ok_or(GraphicsError::InvalidDimensions {
                        protocol: KITTY_PROTOCOL,
                    })?,
                    self.height.ok_or(GraphicsError::InvalidDimensions {
                        protocol: KITTY_PROTOCOL,
                    })?,
                    &bytes,
                )?,
                None,
            ),
            KittyFormat::Rgba => (
                raw_rgba(
                    KITTY_PROTOCOL,
                    self.width.ok_or(GraphicsError::InvalidDimensions {
                        protocol: KITTY_PROTOCOL,
                    })?,
                    self.height.ok_or(GraphicsError::InvalidDimensions {
                        protocol: KITTY_PROTOCOL,
                    })?,
                    &bytes,
                )?,
                None,
            ),
            KittyFormat::Png => {
                if !bytes.starts_with(PNG_SIGNATURE) {
                    return Err(GraphicsError::DecodeFailure {
                        protocol: KITTY_PROTOCOL,
                    });
                }
                (decode_png(KITTY_PROTOCOL, &bytes)?, None)
            }
        };
        Ok(DecodedGraphics {
            query: self.query,
            protocol: KITTY_PROTOCOL,
            image,
            animation,
            action: self.action,
            display: self.display,
        })
    }
}

enum TransferData {
    Encoded(Vec<u8>),
    Ready(Vec<u8>),
}

struct TransferRequest<'a> {
    medium: u8,
    payload: &'a [u8],
    source_offset: Option<usize>,
    source_size: Option<usize>,
    compressed: bool,
    final_chunk: bool,
    format: Option<KittyFormat>,
    width: Option<u32>,
    height: Option<u32>,
}

fn acquire_transfer_data(request: TransferRequest<'_>) -> Result<TransferData, GraphicsError> {
    let TransferRequest {
        medium,
        payload,
        source_offset,
        source_size,
        compressed,
        final_chunk,
        format,
        width,
        height,
    } = request;
    if !final_chunk {
        if medium != b'd' || source_offset.is_some() {
            return Err(GraphicsError::InvalidCommand {
                protocol: KITTY_PROTOCOL,
            });
        }
        return Ok(TransferData::Encoded(payload.to_vec()));
    }

    let bytes = match medium {
        b'd' => decode_base64(KITTY_PROTOCOL, payload)?,
        b'f' | b't' | b's' => read_external_source(medium, payload, source_offset, source_size)?,
        other => {
            return Err(GraphicsError::UnsupportedAction {
                protocol: KITTY_PROTOCOL,
                action: format!("transfer medium {}", other as char),
            })
        }
    };
    let bytes = if medium == b's' && source_size.is_none() {
        exact_shared_memory_payload(
            &bytes,
            format.unwrap_or(KittyFormat::Rgba),
            width,
            height,
            compressed,
        )?
    } else if compressed {
        decompress_bounded(KITTY_PROTOCOL, &bytes)?
    } else {
        bytes
    };
    if medium == b'd' && compressed && format == Some(KittyFormat::Png) {
        if let Some(expected) = source_size {
            if expected != bytes.len() {
                return Err(GraphicsError::DeclaredSizeMismatch {
                    protocol: KITTY_PROTOCOL,
                    expected,
                    actual: bytes.len(),
                });
            }
        }
    }
    Ok(TransferData::Ready(bytes))
}

fn exact_shared_memory_payload(
    bytes: &[u8],
    format: KittyFormat,
    width: Option<u32>,
    height: Option<u32>,
    compressed: bool,
) -> Result<Vec<u8>, GraphicsError> {
    if compressed {
        return decompress_bounded_prefix(KITTY_PROTOCOL, bytes).map(|(decoded, _)| decoded);
    }
    let length = match format {
        KittyFormat::Rgb | KittyFormat::Rgba => {
            let width = width.ok_or(GraphicsError::InvalidDimensions {
                protocol: KITTY_PROTOCOL,
            })?;
            let height = height.ok_or(GraphicsError::InvalidDimensions {
                protocol: KITTY_PROTOCOL,
            })?;
            let channels = if format == KittyFormat::Rgb { 3 } else { 4 };
            usize::try_from(width)
                .ok()
                .and_then(|width| {
                    usize::try_from(height)
                        .ok()
                        .and_then(|height| width.checked_mul(height))
                })
                .and_then(|pixels| pixels.checked_mul(channels))
                .ok_or(GraphicsError::ImageTooLarge {
                    protocol: KITTY_PROTOCOL,
                })?
        }
        KittyFormat::Png => png_stream_len(bytes)?,
    };
    if length > MAX_GRAPHICS_TRANSFER_BYTES {
        return Err(GraphicsError::TransferTooLarge {
            protocol: KITTY_PROTOCOL,
        });
    }
    bytes
        .get(..length)
        .map(<[u8]>::to_vec)
        .ok_or(GraphicsError::DecodeFailure {
            protocol: KITTY_PROTOCOL,
        })
}

fn png_stream_len(bytes: &[u8]) -> Result<usize, GraphicsError> {
    if !bytes.starts_with(PNG_SIGNATURE) {
        return Err(GraphicsError::DecodeFailure {
            protocol: KITTY_PROTOCOL,
        });
    }
    let mut offset = PNG_SIGNATURE.len();
    loop {
        let header =
            bytes
                .get(offset..offset.saturating_add(8))
                .ok_or(GraphicsError::DecodeFailure {
                    protocol: KITTY_PROTOCOL,
                })?;
        let data_len = usize::try_from(u32::from_be_bytes(header[..4].try_into().map_err(
            |_| GraphicsError::DecodeFailure {
                protocol: KITTY_PROTOCOL,
            },
        )?))
        .map_err(|_| GraphicsError::DecodeFailure {
            protocol: KITTY_PROTOCOL,
        })?;
        let end = offset
            .checked_add(12)
            .and_then(|length| length.checked_add(data_len))
            .ok_or(GraphicsError::ImageTooLarge {
                protocol: KITTY_PROTOCOL,
            })?;
        if end > bytes.len() {
            return Err(GraphicsError::DecodeFailure {
                protocol: KITTY_PROTOCOL,
            });
        }
        if &header[4..8] == b"IEND" {
            return Ok(end);
        }
        offset = end;
    }
}

fn validate_transfer_controls(control: &KittyControl, medium: u8) -> Result<(), GraphicsError> {
    if medium != b'd' && control.more {
        return Err(GraphicsError::InvalidCommand {
            protocol: KITTY_PROTOCOL,
        });
    }
    if medium == b'd' && control.source_offset.is_some() {
        return Err(GraphicsError::InvalidCommand {
            protocol: KITTY_PROTOCOL,
        });
    }
    if control.format == Some(KittyFormat::Png)
        && control.compression == Some(true)
        && control.source_size.is_none()
        && medium == b'd'
    {
        return Err(GraphicsError::InvalidHeader {
            protocol: KITTY_PROTOCOL,
        });
    }
    Ok(())
}

fn prepare_animation_payload(
    header: &[u8],
    payload: &[u8],
    final_chunk: bool,
) -> Result<Vec<u8>, GraphicsError> {
    let control = animation_control(header)?;
    let medium = control.medium.unwrap_or(b'd');
    validate_transfer_controls(&control, medium)?;
    let data = acquire_transfer_data(TransferRequest {
        medium,
        payload,
        source_offset: control.source_offset,
        source_size: control.source_size,
        compressed: control.compression == Some(true),
        final_chunk,
        format: control.format,
        width: control.width,
        height: control.height,
    })?;
    match data {
        TransferData::Ready(data) => Ok(STANDARD.encode(data).into_bytes()),
        TransferData::Encoded(_) => Err(GraphicsError::InvalidBase64 {
            protocol: KITTY_PROTOCOL,
        }),
    }
}

fn animation_control(header: &[u8]) -> Result<KittyControl, GraphicsError> {
    let body = header
        .strip_prefix(b"G")
        .ok_or(GraphicsError::InvalidHeader {
            protocol: KITTY_PROTOCOL,
        })?;
    let mut normalized = Vec::new();
    for field in body.split(|byte| *byte == b',') {
        if field.starts_with(b"a=") {
            normalized.extend_from_slice(b"a=t");
        } else {
            normalized.extend_from_slice(field);
        }
        normalized.push(b',');
    }
    if normalized.last() == Some(&b',') {
        normalized.pop();
    }
    parse_kitty_control(&normalized)
}

fn read_external_source(
    medium: u8,
    encoded_name: &[u8],
    source_offset: Option<usize>,
    source_size: Option<usize>,
) -> Result<Vec<u8>, GraphicsError> {
    let name = decode_base64(KITTY_PROTOCOL, encoded_name)?;
    if name.is_empty() || name.len() > MAX_GRAPHICS_TRANSFER_BYTES {
        return Err(GraphicsError::InvalidHeader {
            protocol: KITTY_PROTOCOL,
        });
    }
    let offset = source_offset.unwrap_or(0);
    match medium {
        b'f' => read_regular_file(&name, offset, source_size, false),
        b't' => read_regular_file(&name, offset, source_size, true),
        b's' => read_shared_memory(&name, offset, source_size),
        _ => Err(GraphicsError::UnsupportedAction {
            protocol: KITTY_PROTOCOL,
            action: format!("transfer medium {}", medium as char),
        }),
    }
}

fn read_regular_file(
    name: &[u8],
    offset: usize,
    size: Option<usize>,
    delete: bool,
) -> Result<Vec<u8>, GraphicsError> {
    let supplied_path = path_from_bytes(name)?;
    let path = if delete {
        disposable_file_path(&supplied_path).ok_or_else(external_source_error)?
    } else {
        supplied_path
    };
    let mut file = open_regular_source(&path)?;
    let result = file
        .metadata()
        .map_err(|_| external_source_error())
        .and_then(|metadata| {
            if metadata.file_type().is_file() {
                read_file_range(&mut file, offset, size)
            } else {
                Err(external_source_error())
            }
        });
    drop(file);
    if delete {
        let removed = fs::remove_file(&path).is_ok();
        if !removed {
            return Err(external_source_error());
        }
    }
    result
}

#[cfg(unix)]
fn open_regular_source(path: &Path) -> Result<File, GraphicsError> {
    use std::os::unix::fs::OpenOptionsExt;

    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)
        .map_err(|_| external_source_error())
}

#[cfg(not(unix))]
fn open_regular_source(path: &Path) -> Result<File, GraphicsError> {
    File::open(path).map_err(|_| external_source_error())
}

fn read_file_range(
    file: &mut File,
    offset: usize,
    size: Option<usize>,
) -> Result<Vec<u8>, GraphicsError> {
    let file_size = file.metadata().map_err(|_| external_source_error())?.len();
    let offset = u64::try_from(offset).map_err(|_| external_source_error())?;
    if offset > file_size {
        return Err(external_source_error());
    }
    let available = file_size - offset;
    let length = match size {
        Some(size) => {
            if u64::try_from(size).map_err(|_| external_source_error())? > available {
                return Err(external_source_error());
            }
            size
        }
        None => usize::try_from(available).map_err(|_| external_source_too_large())?,
    };
    if length > MAX_GRAPHICS_TRANSFER_BYTES {
        return Err(external_source_too_large());
    }
    file.seek(SeekFrom::Start(offset))
        .map_err(|_| external_source_error())?;
    let mut data = Vec::new();
    data.try_reserve_exact(length)
        .map_err(|_| external_source_too_large())?;
    let mut remaining = length;
    let mut buffer = [0u8; 8192];
    while remaining != 0 {
        let read_length = remaining.min(buffer.len());
        let count = file
            .read(&mut buffer[..read_length])
            .map_err(|_| external_source_error())?;
        if count == 0 {
            return Err(external_source_error());
        }
        data.extend_from_slice(&buffer[..count]);
        remaining -= count;
    }
    Ok(data)
}

fn disposable_file_path(path: &Path) -> Option<PathBuf> {
    let canonical = fs::canonicalize(path).ok()?;
    let name = canonical.to_string_lossy().to_ascii_lowercase();
    if !name.contains("tty-graphics-protocol") {
        return None;
    }
    let mut roots = Vec::new();
    if let Ok(root) = fs::canonicalize(std::env::temp_dir()) {
        roots.push(root);
    }
    #[cfg(unix)]
    {
        for root in [Path::new("/tmp"), Path::new("/dev/shm")] {
            if let Ok(root) = fs::canonicalize(root) {
                roots.push(root);
            }
        }
    }
    roots
        .iter()
        .any(|root| canonical.starts_with(root))
        .then_some(canonical)
}

fn path_from_bytes(bytes: &[u8]) -> Result<PathBuf, GraphicsError> {
    #[cfg(unix)]
    {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        if bytes.contains(&0) {
            return Err(external_source_error());
        }
        Ok(PathBuf::from(OsString::from_vec(bytes.to_vec())))
    }
    #[cfg(not(unix))]
    {
        String::from_utf8(bytes.to_vec())
            .map(PathBuf::from)
            .map_err(|_| external_source_error())
    }
}

#[cfg(unix)]
fn read_shared_memory(
    name: &[u8],
    offset: usize,
    size: Option<usize>,
) -> Result<Vec<u8>, GraphicsError> {
    let name = CString::new(name).map_err(|_| external_source_error())?;
    let descriptor = unsafe { libc::shm_open(name.as_ptr(), libc::O_RDONLY, 0) };
    if descriptor < 0 {
        return Err(external_source_error());
    }
    let mut file = unsafe { File::from_raw_fd(descriptor) };
    let result = read_file_range(&mut file, offset, size);
    drop(file);
    let unlinked = unsafe { libc::shm_unlink(name.as_ptr()) } == 0;
    if !unlinked && result.is_ok() {
        return Err(external_source_error());
    }
    result
}

#[cfg(windows)]
fn read_shared_memory(
    name: &[u8],
    offset: usize,
    size: Option<usize>,
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

    let name = String::from_utf8(name.to_vec()).map_err(|_| external_source_error())?;
    let mut wide = name.encode_utf16().collect::<Vec<_>>();
    wide.push(0);
    let mapping = unsafe { OpenFileMappingW(FILE_MAP_READ, 0, wide.as_ptr()) };
    if mapping.is_null() {
        return Err(external_source_error());
    }
    let result = (|| {
        let offset = u64::try_from(offset).map_err(|_| external_source_error())?;
        let mut system_info = SYSTEM_INFO::default();
        unsafe { GetSystemInfo(&mut system_info) };
        let granularity = u64::from(system_info.dwAllocationGranularity);
        if granularity == 0 {
            return Err(external_source_error());
        }
        let aligned = offset - offset % granularity;
        let delta = usize::try_from(offset - aligned).map_err(|_| external_source_error())?;
        if size.is_some_and(|size| size > MAX_GRAPHICS_TRANSFER_BYTES) {
            return Err(external_source_too_large());
        }
        let requested = match size {
            Some(size) => Some(delta.checked_add(size).ok_or_else(external_source_error)?),
            None => None,
        };
        let view = unsafe {
            MapViewOfFile(
                mapping,
                FILE_MAP_READ,
                u32::try_from(aligned >> 32).map_err(|_| external_source_error())?,
                u32::try_from(aligned).map_err(|_| external_source_error())?,
                requested.unwrap_or(0),
            )
        };
        if view.Value.is_null() {
            return Err(external_source_error());
        }
        let result = (|| {
            let length = if let Some(size) = size {
                size
            } else {
                let mut information = MEMORY_BASIC_INFORMATION::default();
                let queried = unsafe {
                    VirtualQuery(
                        view.Value as *const c_void,
                        &mut information,
                        size_of::<MEMORY_BASIC_INFORMATION>(),
                    )
                };
                if queried == 0 || delta > information.RegionSize {
                    return Err(external_source_error());
                }
                (information.RegionSize - delta).min(MAX_GRAPHICS_TRANSFER_BYTES)
            };
            let mut data = Vec::new();
            data.try_reserve_exact(length)
                .map_err(|_| external_source_too_large())?;
            data.resize(length, 0);
            let source = unsafe { (view.Value as *const u8).add(delta) };
            unsafe { ptr::copy_nonoverlapping(source, data.as_mut_ptr(), length) };
            Ok(data)
        })();
        unsafe { UnmapViewOfFile(view) };
        result
    })();
    unsafe { CloseHandle(mapping) };
    result
}

#[cfg(not(any(unix, windows)))]
fn read_shared_memory(
    _name: &[u8],
    _offset: usize,
    _size: Option<usize>,
) -> Result<Vec<u8>, GraphicsError> {
    Err(external_source_error())
}

fn external_source_error() -> GraphicsError {
    GraphicsError::DecodeFailure {
        protocol: KITTY_PROTOCOL,
    }
}

fn external_source_too_large() -> GraphicsError {
    GraphicsError::TransferTooLarge {
        protocol: KITTY_PROTOCOL,
    }
}

impl KittyAnimationTransfer {
    /// Return display metadata from the first animation-frame chunk.
    #[must_use]
    pub fn display(&self) -> ImageDisplay {
        reply_display(&self.header)
    }

    /// Validate and append a continuation chunk.
    pub fn accept_chunk(
        mut self,
        chunk: KittyAnimationChunk,
    ) -> Result<KittyAnimationTransferOutcome, GraphicsError> {
        if !chunk.continuation() {
            return Err(GraphicsError::InvalidHeader {
                protocol: KITTY_PROTOCOL,
            });
        }
        let header = chunk.header();
        let body = header
            .strip_prefix(b"G")
            .ok_or(GraphicsError::InvalidHeader {
                protocol: KITTY_PROTOCOL,
            })?;
        if !body
            .split(|byte| *byte == b',')
            .filter(|field| !field.is_empty())
            .all(|field| {
                field.starts_with(b"a=f") || field.starts_with(b"m=") || field.starts_with(b"q=")
            })
        {
            return Err(GraphicsError::InvalidHeader {
                protocol: KITTY_PROTOCOL,
            });
        }
        append_bounded(&mut self.encoded, chunk.payload())?;
        if chunk.more() {
            Ok(KittyAnimationTransferOutcome::Pending(self))
        } else {
            let payload = prepare_animation_payload(&self.header, &self.encoded, true)?;
            let body = self
                .header
                .strip_prefix(b"G")
                .ok_or(GraphicsError::InvalidHeader {
                    protocol: KITTY_PROTOCOL,
                })?;
            let command = commands::parse_animation_command_fields(
                body,
                &payload,
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
    pub(super) query: bool,
    pub(super) id: Option<u32>,
    pub(super) action: ImageAction,
    pub(super) medium: Option<u8>,
    pub(super) format: Option<KittyFormat>,
    pub(super) width: Option<u32>,
    pub(super) height: Option<u32>,
    pub(super) more: bool,
    pub(super) more_specified: bool,
    pub(super) compression: Option<bool>,
    pub(super) source_size: Option<usize>,
    pub(super) source_offset: Option<usize>,
    pub(super) display: ImageDisplay,
    pub(super) continuation_compatible: bool,
}

impl Default for KittyControl {
    fn default() -> Self {
        KittyControl {
            query: false,
            id: None,
            action: ImageAction::Transmit,
            medium: None,
            format: None,
            width: None,
            height: None,
            more: false,
            more_specified: false,
            compression: None,
            source_size: None,
            source_offset: None,
            display: ImageDisplay::default(),
            continuation_compatible: true,
        }
    }
}

pub(super) fn parse_kitty_control(data: &[u8]) -> Result<KittyControl, GraphicsError> {
    let mut control = KittyControl::default();
    let mut seen = [false; 256];
    for field in data.split(|byte| *byte == b',') {
        if field.is_empty() {
            continue;
        }
        let (key, value) = split_at_byte(field, b'=').ok_or(GraphicsError::InvalidHeader {
            protocol: KITTY_PROTOCOL,
        })?;
        if key.len() != 1 || value.is_empty() {
            return Err(GraphicsError::InvalidHeader {
                protocol: KITTY_PROTOCOL,
            });
        }
        let key = key[0];
        let slot = match key {
            b'a' | b'C' | b'c' | b'd' | b'f' | b'h' | b'H' | b'i' | b'I' | b'm' | b'N' | b'o'
            | b'O' | b'p' | b'P' | b'q' | b'Q' | b'r' | b's' | b'S' | b't' | b'U' | b'v' | b'V'
            | b'w' | b'x' | b'X' | b'y' | b'Y' | b'z' => usize::from(key),
            _ => {
                return Err(GraphicsError::InvalidHeader {
                    protocol: KITTY_PROTOCOL,
                })
            }
        };
        if seen[slot] {
            return Err(GraphicsError::InvalidHeader {
                protocol: KITTY_PROTOCOL,
            });
        }
        seen[slot] = true;
        if !matches!(key, b'm' | b'q') {
            control.continuation_compatible = false;
        }
        match key {
            b'a' => {
                let action = as_ascii(value)?;
                control.action = match value {
                    b"t" => ImageAction::Transmit,
                    b"T" => ImageAction::TransmitAndDisplay,
                    b"q" => {
                        control.query = true;
                        ImageAction::Transmit
                    }
                    _ => {
                        return Err(GraphicsError::UnsupportedAction {
                            protocol: KITTY_PROTOCOL,
                            action,
                        })
                    }
                };
            }
            b'f' => {
                control.format = Some(match parse_u32(value)? {
                    24 => KittyFormat::Rgb,
                    32 => KittyFormat::Rgba,
                    100 => KittyFormat::Png,
                    _ => {
                        return Err(GraphicsError::UnsupportedMedia {
                            protocol: KITTY_PROTOCOL,
                            format: String::from_utf8_lossy(value).into_owned(),
                        })
                    }
                });
            }
            b'h' => {
                let height = parse_u32(value)?;
                if height == 0 {
                    return Err(GraphicsError::InvalidDimensions {
                        protocol: KITTY_PROTOCOL,
                    });
                }
                control.display.height = Some(koshi_image::ImageDimension::Pixels(height));
            }
            b'i' => {
                if control.display.image_number.is_some() {
                    return Err(GraphicsError::InvalidHeader {
                        protocol: KITTY_PROTOCOL,
                    });
                }
                let id = parse_u32(value)?;
                if id == 0 {
                    return Err(GraphicsError::InvalidCommand {
                        protocol: KITTY_PROTOCOL,
                    });
                }
                control.id = Some(id);
                control.display.image_id = Some(id);
            }
            b'I' => {
                if control.id.is_some() {
                    return Err(GraphicsError::InvalidHeader {
                        protocol: KITTY_PROTOCOL,
                    });
                }
                let id = parse_u32(value)?;
                if id == 0 {
                    return Err(GraphicsError::InvalidCommand {
                        protocol: KITTY_PROTOCOL,
                    });
                }
                control.display.image_number = Some(id);
            }
            b'm' => match value {
                b"0" => {
                    control.more = false;
                    control.more_specified = true;
                }
                b"1" => {
                    control.more = true;
                    control.more_specified = true;
                }
                _ => {
                    return Err(GraphicsError::InvalidCommand {
                        protocol: KITTY_PROTOCOL,
                    })
                }
            },
            b'o' => {
                control.compression = Some(match value {
                    b"z" => true,
                    _ => {
                        return Err(GraphicsError::UnsupportedMedia {
                            protocol: KITTY_PROTOCOL,
                            format: as_ascii(value)?,
                        })
                    }
                });
            }
            b's' => control.width = Some(parse_u32(value)?),
            b'v' => control.height = Some(parse_u32(value)?),
            b'p' => {
                control.display.placement_id = Some(parse_u32(value)?);
            }
            b'S' => control.source_size = Some(parse_usize(value)?),
            b'O' => control.source_offset = Some(parse_usize(value)?),
            b't' => {
                if value.len() != 1 {
                    return Err(GraphicsError::InvalidCommand {
                        protocol: KITTY_PROTOCOL,
                    });
                }
                control.medium = Some(value[0]);
            }
            b'c' => {
                control.display.cell_columns = Some(parse_positive_u32(value)?);
            }
            b'r' => {
                control.display.cell_rows = Some(parse_positive_u32(value)?);
            }
            b'w' => {
                control.display.width = Some(koshi_image::ImageDimension::Pixels(
                    parse_positive_u32(value)?,
                ));
            }
            b'x' => {
                control.display.source_offset_x = Some(parse_u32(value)?);
            }
            b'y' => {
                control.display.source_offset_y = Some(parse_u32(value)?);
            }
            b'X' => {
                control.display.cell_offset_x = Some(parse_u32(value)?);
            }
            b'Y' => {
                control.display.cell_offset_y = Some(parse_u32(value)?);
            }
            b'C' => {
                control.display.move_cursor = match parse_u32(value)? {
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
                control.display.usage_hints = parse_u32(value)?;
            }
            b'U' => {
                control.display.unicode_placeholder = match parse_u32(value)? {
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
                control.display.relative_image_id = Some(parse_positive_u32(value)?);
            }
            b'Q' => {
                control.display.relative_placement_id = Some(parse_positive_u32(value)?);
            }
            b'H' => {
                control.display.relative_offset_x = parse_i32(value)?;
            }
            b'V' => {
                control.display.relative_offset_y = parse_i32(value)?;
            }
            b'z' => {
                control.display.z_index = parse_i32(value)?;
            }
            b'q' => {
                let quiet = parse_u32(value)?;
                control.display.quiet = quiet.min(2) as u8;
                if quiet > 2 {
                    return Err(GraphicsError::InvalidCommand {
                        protocol: KITTY_PROTOCOL,
                    });
                }
            }
            b'd' => {
                return Err(GraphicsError::UnsupportedAction {
                    protocol: KITTY_PROTOCOL,
                    action: format!("control {}", key as char),
                });
            }
            _ => unreachable!(),
        }
    }
    Ok(control)
}

fn validate_kitty_continuation(
    transfer: &KittyTransfer,
    chunk: &KittyChunk,
) -> Result<(), GraphicsError> {
    if !chunk.more_specified || !chunk.continuation_compatible {
        return Err(GraphicsError::InvalidHeader {
            protocol: KITTY_PROTOCOL,
        });
    }
    if let Some(id) = chunk.id {
        if transfer.id != Some(id) {
            return Err(GraphicsError::InvalidHeader {
                protocol: KITTY_PROTOCOL,
            });
        }
    }
    if let Some(format) = chunk.format {
        if format != transfer.format {
            return Err(GraphicsError::InvalidHeader {
                protocol: KITTY_PROTOCOL,
            });
        }
    }
    if let Some(width) = chunk.width {
        if transfer.width != Some(width) {
            return Err(GraphicsError::InvalidDimensions {
                protocol: KITTY_PROTOCOL,
            });
        }
    }
    if let Some(height) = chunk.height {
        if transfer.height != Some(height) {
            return Err(GraphicsError::InvalidDimensions {
                protocol: KITTY_PROTOCOL,
            });
        }
    }
    if let Some(compression) = chunk.compression {
        if compression != transfer.compression {
            return Err(GraphicsError::InvalidCommand {
                protocol: KITTY_PROTOCOL,
            });
        }
    }
    if let Some(expected) = chunk.declared_size {
        if transfer.declared_size.is_some() && transfer.declared_size != Some(expected) {
            return Err(GraphicsError::InvalidHeader {
                protocol: KITTY_PROTOCOL,
            });
        }
    }
    Ok(())
}

fn append_bounded(target: &mut Vec<u8>, bytes: &[u8]) -> Result<(), GraphicsError> {
    let new_len =
        target
            .len()
            .checked_add(bytes.len())
            .ok_or(GraphicsError::InvalidDimensions {
                protocol: KITTY_PROTOCOL,
            })?;
    if new_len > MAX_GRAPHICS_TRANSFER_BYTES {
        return Err(GraphicsError::TransferTooLarge {
            protocol: KITTY_PROTOCOL,
        });
    }
    target.extend_from_slice(bytes);
    Ok(())
}

fn push_bounded(target: &mut Vec<u8>, byte: u8, limit: usize) -> Result<(), GraphicsError> {
    if target.len() == limit {
        return Err(GraphicsError::TransferTooLarge {
            protocol: KITTY_PROTOCOL,
        });
    }
    target.push(byte);
    Ok(())
}

fn parse_u32(data: &[u8]) -> Result<u32, GraphicsError> {
    if data.is_empty() || !data.iter().all(u8::is_ascii_digit) {
        return Err(GraphicsError::InvalidCommand {
            protocol: KITTY_PROTOCOL,
        });
    }
    let mut value = 0u32;
    for &byte in data {
        value = value
            .checked_mul(10)
            .and_then(|value| value.checked_add(u32::from(byte - b'0')))
            .ok_or(GraphicsError::InvalidDimensions {
                protocol: KITTY_PROTOCOL,
            })?;
    }
    Ok(value)
}

fn parse_i32(data: &[u8]) -> Result<i32, GraphicsError> {
    if data.is_empty() {
        return Err(GraphicsError::InvalidCommand {
            protocol: KITTY_PROTOCOL,
        });
    }
    let (negative, digits) = match data.first() {
        Some(b'-') => (true, &data[1..]),
        _ => (false, data),
    };
    if digits.is_empty() || !digits.iter().all(u8::is_ascii_digit) {
        return Err(GraphicsError::InvalidCommand {
            protocol: KITTY_PROTOCOL,
        });
    }
    let mut value = 0u32;
    for &byte in digits {
        value = value
            .checked_mul(10)
            .and_then(|value| value.checked_add(u32::from(byte - b'0')))
            .ok_or(GraphicsError::InvalidCommand {
                protocol: KITTY_PROTOCOL,
            })?;
    }
    if negative {
        if value == 2_147_483_648 {
            Ok(i32::MIN)
        } else {
            i32::try_from(value)
                .ok()
                .and_then(|value| value.checked_neg())
                .ok_or(GraphicsError::InvalidCommand {
                    protocol: KITTY_PROTOCOL,
                })
        }
    } else {
        i32::try_from(value).map_err(|_| GraphicsError::InvalidCommand {
            protocol: KITTY_PROTOCOL,
        })
    }
}

fn parse_positive_u32(data: &[u8]) -> Result<u32, GraphicsError> {
    let value = parse_u32(data)?;
    if value == 0 {
        return Err(GraphicsError::InvalidDimensions {
            protocol: KITTY_PROTOCOL,
        });
    }
    Ok(value)
}

fn parse_usize(data: &[u8]) -> Result<usize, GraphicsError> {
    let value = parse_u32(data)?;
    usize::try_from(value).map_err(|_| GraphicsError::InvalidDimensions {
        protocol: KITTY_PROTOCOL,
    })
}

fn as_ascii(data: &[u8]) -> Result<String, GraphicsError> {
    if !data.is_ascii() {
        return Err(GraphicsError::InvalidHeader {
            protocol: KITTY_PROTOCOL,
        });
    }
    Ok(String::from_utf8_lossy(data).into_owned())
}

fn split_at_byte(data: &[u8], delimiter: u8) -> Option<(&[u8], &[u8])> {
    let index = data.iter().position(|byte| *byte == delimiter)?;
    Some((&data[..index], &data[index + 1..]))
}

#[cfg(test)]
mod tests;
