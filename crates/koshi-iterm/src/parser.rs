//! iTerm2 OSC 1337 command parsing and multipart state.

use koshi_image::{
    decode_base64, decode_media, DecodedGraphics, DecodedMedia, GraphicsError, GraphicsProtocol,
    ImageAction, ImageDimension, ImageDisplay, MAX_GRAPHICS_CONTROL_BYTES,
    MAX_GRAPHICS_TRANSFER_BYTES,
};

const ITERM_PROTOCOL: GraphicsProtocol = GraphicsProtocol::Iterm2;

/// A multipart iTerm2 image transfer that is waiting for more commands.
///
/// The transfer stores the validated display metadata and the encoded payload
/// bytes received so far. Only the terminal parser needs to retain this value;
/// callers start one with [`parse_iterm_command`] and `None`.
#[derive(Clone, PartialEq, Eq)]
pub struct ItermTransfer {
    meta: ItermMeta,
    encoded: Vec<u8>,
}

impl std::fmt::Debug for ItermTransfer {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ItermTransfer")
            .field("meta", &self.meta)
            .field("encoded_len", &self.encoded.len())
            .finish()
    }
}

fn iterm_command_name(body: &[u8]) -> Option<&[u8]> {
    (!body.is_empty()).then(|| split_at_byte(body, b'=').map_or(body, |(command, _)| command))
}

const ITERM_GRAPHICS_COMMANDS: [&[u8]; 4] = [b"File", b"MultipartFile", b"FilePart", b"FileEnd"];

/// Return whether an OSC 1337 body is an exact iTerm2 graphics command.
pub fn iterm_command_is_graphics(body: &[u8]) -> bool {
    match iterm_command_name(body) {
        None => true,
        Some(command) => ITERM_GRAPHICS_COMMANDS.contains(&command),
    }
}

/// Return whether an OSC 1337 body names or prefixes a graphics command.
pub fn iterm_command_can_be_graphics(body: &[u8]) -> bool {
    let command = iterm_command_name(body).unwrap_or(body);
    ITERM_GRAPHICS_COMMANDS
        .iter()
        .any(|name| name.starts_with(command))
}

/// Return whether an iTerm2 body has reached an image payload.
pub fn iterm_payload_started(body: &[u8]) -> bool {
    if body.starts_with(b"FilePart=") {
        return true;
    }
    matches!(iterm_command_name(body), Some(b"File" | b"MultipartFile")) && body.contains(&b':')
}

/// Parse one complete iTerm2 OSC 1337 body.
///
/// `multipart` is the caller-owned state for `MultipartFile`, `FilePart`, and
/// `FileEnd`. A rejected command leaves that state unchanged unless the command
/// is a valid `FileEnd`, which consumes the completed transfer before decoding.
pub fn parse_iterm_command(
    body: &[u8],
    multipart: &mut Option<ItermTransfer>,
) -> Result<Option<DecodedGraphics>, GraphicsError> {
    if body.is_empty() {
        return Err(GraphicsError::InvalidHeader {
            protocol: ITERM_PROTOCOL,
        });
    }
    let (command, rest) = split_at_byte(body, b'=').unwrap_or((body, &[]));
    match command {
        b"File" => parse_file(rest, multipart),
        b"MultipartFile" => parse_multipart_file(rest, multipart),
        b"FilePart" => parse_file_part(rest, multipart),
        b"FileEnd" => parse_file_end(rest, multipart),
        _ => Ok(None),
    }
}

fn parse_file(
    rest: &[u8],
    multipart: &Option<ItermTransfer>,
) -> Result<Option<DecodedGraphics>, GraphicsError> {
    if multipart.is_some() {
        return Err(GraphicsError::MultipartState);
    }
    let (params, encoded) = split_at_byte(rest, b':').ok_or(GraphicsError::InvalidHeader {
        protocol: ITERM_PROTOCOL,
    })?;
    let meta = parse_iterm_meta(params, true)?;
    let bytes = decode_base64(ITERM_PROTOCOL, encoded)?;
    Ok(Some(decoded_graphics(meta, &bytes)?))
}

fn parse_multipart_file(
    rest: &[u8],
    multipart: &mut Option<ItermTransfer>,
) -> Result<Option<DecodedGraphics>, GraphicsError> {
    if multipart.is_some() {
        return Err(GraphicsError::MultipartState);
    }
    let (params, encoded) = split_at_byte(rest, b':').unwrap_or((rest, &[]));
    let meta = parse_iterm_meta(params, true)?;
    if encoded.len() > MAX_GRAPHICS_TRANSFER_BYTES {
        return Err(GraphicsError::TransferTooLarge {
            protocol: ITERM_PROTOCOL,
        });
    }
    *multipart = Some(ItermTransfer {
        meta,
        encoded: encoded.to_vec(),
    });
    Ok(None)
}

fn parse_file_part(
    rest: &[u8],
    multipart: &mut Option<ItermTransfer>,
) -> Result<Option<DecodedGraphics>, GraphicsError> {
    let transfer = multipart.as_mut().ok_or(GraphicsError::MultipartState)?;
    append_bounded(&mut transfer.encoded, rest)?;
    Ok(None)
}

fn parse_file_end(
    rest: &[u8],
    multipart: &mut Option<ItermTransfer>,
) -> Result<Option<DecodedGraphics>, GraphicsError> {
    if !rest.is_empty() {
        return Err(GraphicsError::InvalidHeader {
            protocol: ITERM_PROTOCOL,
        });
    }
    let transfer = multipart.take().ok_or(GraphicsError::MultipartState)?;
    let bytes = decode_base64(ITERM_PROTOCOL, &transfer.encoded)?;
    Ok(Some(decoded_graphics(transfer.meta, &bytes)?))
}

fn decoded_graphics(meta: ItermMeta, bytes: &[u8]) -> Result<DecodedGraphics, GraphicsError> {
    let (image, animation) = match decode_media(ITERM_PROTOCOL, bytes)? {
        DecodedMedia::Static(image) => (image, None),
        DecodedMedia::Animation(animation) => {
            let image = animation.frames()[0].image().clone();
            (image, Some(animation))
        }
    };
    Ok(DecodedGraphics {
        query: false,
        protocol: ITERM_PROTOCOL,
        image,
        animation,
        action: ImageAction::Display,
        display: meta.display,
    })
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ItermMeta {
    display: ImageDisplay,
}

fn parse_iterm_meta(data: &[u8], require_inline: bool) -> Result<ItermMeta, GraphicsError> {
    if data.len() > MAX_GRAPHICS_CONTROL_BYTES {
        return Err(GraphicsError::TransferTooLarge {
            protocol: ITERM_PROTOCOL,
        });
    }
    let mut display = ImageDisplay::default();
    let mut inline = false;
    for field in data.split(|byte| *byte == b';') {
        if field.is_empty() {
            continue;
        }
        let (key, value) = split_at_byte(field, b'=').ok_or(GraphicsError::InvalidHeader {
            protocol: ITERM_PROTOCOL,
        })?;
        match key {
            b"inline" => match value {
                b"1" => inline = true,
                b"0" => inline = false,
                _ => {
                    return Err(GraphicsError::InvalidCommand {
                        protocol: ITERM_PROTOCOL,
                    })
                }
            },
            b"size" => {
                check_ascii_decimal(value)?;
            }
            b"width" => display.width = Some(parse_iterm_dimension(value)?),
            b"height" => display.height = Some(parse_iterm_dimension(value)?),
            b"preserveAspectRatio" => match value {
                b"1" => display.preserve_aspect_ratio = true,
                b"0" => display.preserve_aspect_ratio = false,
                _ => {
                    return Err(GraphicsError::InvalidCommand {
                        protocol: ITERM_PROTOCOL,
                    })
                }
            },
            b"name" => check_control_value(value)?,
            _ => check_control_value(value)?,
        }
    }
    if require_inline && !inline {
        return Err(GraphicsError::UnsupportedAction {
            protocol: ITERM_PROTOCOL,
            action: "inline=0".to_string(),
        });
    }
    Ok(ItermMeta { display })
}

fn check_control_value(value: &[u8]) -> Result<(), GraphicsError> {
    if value.len() > MAX_GRAPHICS_CONTROL_BYTES {
        return Err(GraphicsError::TransferTooLarge {
            protocol: ITERM_PROTOCOL,
        });
    }
    Ok(())
}

fn parse_iterm_dimension(value: &[u8]) -> Result<ImageDimension, GraphicsError> {
    if value == b"auto" {
        return Ok(ImageDimension::Auto);
    }
    if value.ends_with(b"px") {
        return match parse_signed_decimal(&value[..value.len().saturating_sub(2)])? {
            SignedDecimal::NonPositive => Ok(ImageDimension::Pixels(1)),
            SignedDecimal::Positive(number) => Ok(ImageDimension::Pixels(number)),
            SignedDecimal::TooLarge => Err(invalid_dimensions()),
        };
    }
    if value.ends_with(b"%") {
        return match parse_signed_decimal(&value[..value.len().saturating_sub(1)])? {
            SignedDecimal::NonPositive => Ok(ImageDimension::Cells(1)),
            SignedDecimal::Positive(number) => Ok(ImageDimension::Percent(
                u16::try_from(number.min(100)).expect("a clamped percentage fits in u16"),
            )),
            SignedDecimal::TooLarge => Ok(ImageDimension::Percent(100)),
        };
    }
    match parse_signed_decimal(value)? {
        SignedDecimal::NonPositive => Ok(ImageDimension::Cells(1)),
        SignedDecimal::Positive(number) => Ok(ImageDimension::Cells(number)),
        SignedDecimal::TooLarge => Err(invalid_dimensions()),
    }
}

#[derive(Clone, Copy)]
enum SignedDecimal {
    NonPositive,
    Positive(u32),
    TooLarge,
}

fn parse_signed_decimal(data: &[u8]) -> Result<SignedDecimal, GraphicsError> {
    let (negative, digits) = match data {
        [b'-', digits @ ..] => (true, digits),
        [b'+', digits @ ..] => (false, digits),
        digits => (false, digits),
    };
    check_ascii_decimal(digits)?;
    let mut value = 0u32;
    for &byte in digits {
        let Some(next) = value
            .checked_mul(10)
            .and_then(|value| value.checked_add(u32::from(byte - b'0')))
        else {
            return Ok(if negative {
                SignedDecimal::NonPositive
            } else {
                SignedDecimal::TooLarge
            });
        };
        value = next;
    }
    if negative || value == 0 {
        Ok(SignedDecimal::NonPositive)
    } else {
        Ok(SignedDecimal::Positive(value))
    }
}

fn check_ascii_decimal(data: &[u8]) -> Result<(), GraphicsError> {
    if data.is_empty() || !data.iter().all(u8::is_ascii_digit) {
        return Err(GraphicsError::InvalidCommand {
            protocol: ITERM_PROTOCOL,
        });
    }
    Ok(())
}

fn invalid_dimensions() -> GraphicsError {
    GraphicsError::InvalidDimensions {
        protocol: ITERM_PROTOCOL,
    }
}

fn append_bounded(target: &mut Vec<u8>, bytes: &[u8]) -> Result<(), GraphicsError> {
    let new_len =
        target
            .len()
            .checked_add(bytes.len())
            .ok_or(GraphicsError::InvalidDimensions {
                protocol: ITERM_PROTOCOL,
            })?;
    if new_len > MAX_GRAPHICS_TRANSFER_BYTES {
        return Err(GraphicsError::TransferTooLarge {
            protocol: ITERM_PROTOCOL,
        });
    }
    target.extend_from_slice(bytes);
    Ok(())
}

fn split_at_byte(data: &[u8], delimiter: u8) -> Option<(&[u8], &[u8])> {
    let index = data.iter().position(|byte| *byte == delimiter)?;
    Some((&data[..index], &data[index + 1..]))
}

#[cfg(test)]
mod tests;
