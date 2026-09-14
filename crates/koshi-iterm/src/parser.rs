//! iTerm2 OSC 1337 command parsing and multipart state.

use koshi_image::{
    decode_base64, decode_media, DecodedGraphics, DecodedMedia, GraphicsError, GraphicsProtocol,
    ImageAction, ImageDimension, ImageDisplay, MAX_GRAPHICS_CONTROL_BYTE_COUNT,
    MAX_GRAPHICS_TRANSFER_BYTE_COUNT,
};

const ITERM2_PROTOCOL: GraphicsProtocol = GraphicsProtocol::Iterm2;

/// A multipart iTerm2 image transfer that is waiting for more commands.
///
/// The transfer stores the validated display options and the encoded payload
/// bytes received so far. Only the terminal parser needs to retain this value;
/// callers start one with [`parse_iterm_command`] and `None`.
#[derive(Clone, PartialEq, Eq)]
pub struct ItermTransfer {
    display_options: ItermDisplayOptions,
    encoded_payload_bytes: Vec<u8>,
}

impl std::fmt::Debug for ItermTransfer {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ItermTransfer")
            .field("display_options", &self.display_options)
            .field(
                "encoded_payload_byte_count",
                &self.encoded_payload_bytes.len(),
            )
            .finish()
    }
}

fn find_iterm_command_name(command_body: &[u8]) -> Option<&[u8]> {
    (!command_body.is_empty()).then(|| {
        split_bytes_at_delimiter(command_body, b'=')
            .map_or(command_body, |(command_name, _)| command_name)
    })
}

const ITERM_GRAPHICS_COMMAND_NAMES: [&[u8]; 4] =
    [b"File", b"MultipartFile", b"FilePart", b"FileEnd"];

/// Return whether an OSC 1337 body is an exact iTerm2 graphics command.
pub fn is_iterm_graphics_command(command_body: &[u8]) -> bool {
    match find_iterm_command_name(command_body) {
        None => true,
        Some(command_name) => ITERM_GRAPHICS_COMMAND_NAMES.contains(&command_name),
    }
}

/// Return whether an OSC 1337 body names or prefixes a graphics command.
pub fn can_iterm_command_be_graphics(command_body: &[u8]) -> bool {
    let command_name = find_iterm_command_name(command_body).unwrap_or(command_body);
    ITERM_GRAPHICS_COMMAND_NAMES
        .iter()
        .any(|command_name_candidate| command_name_candidate.starts_with(command_name))
}

/// Return whether an iTerm2 body has reached an image payload.
pub fn is_iterm_payload_started(command_body: &[u8]) -> bool {
    if command_body.starts_with(b"FilePart=") {
        return true;
    }
    matches!(
        find_iterm_command_name(command_body),
        Some(b"File" | b"MultipartFile")
    ) && command_body.contains(&b':')
}

/// Parse one complete iTerm2 OSC 1337 body.
///
/// `multipart_transfer` is the caller-owned state for `MultipartFile`, `FilePart`, and
/// `FileEnd`. A rejected command leaves that state unchanged unless the command
/// is a valid `FileEnd`, which consumes the completed transfer before decoding.
pub fn parse_iterm_command(
    command_body: &[u8],
    multipart_transfer: &mut Option<ItermTransfer>,
) -> Result<Option<DecodedGraphics>, GraphicsError> {
    if command_body.is_empty() {
        return Err(GraphicsError::InvalidHeader {
            protocol: ITERM2_PROTOCOL,
        });
    }
    let (command_name, command_payload) =
        split_bytes_at_delimiter(command_body, b'=').unwrap_or((command_body, &[]));
    match command_name {
        b"File" => parse_file_command(command_payload, multipart_transfer),
        b"MultipartFile" => parse_multipart_file_command(command_payload, multipart_transfer),
        b"FilePart" => parse_file_part_command(command_payload, multipart_transfer),
        b"FileEnd" => parse_file_end_command(command_payload, multipart_transfer),
        _ => Ok(None),
    }
}

fn parse_file_command(
    command_payload: &[u8],
    multipart_transfer: &Option<ItermTransfer>,
) -> Result<Option<DecodedGraphics>, GraphicsError> {
    if multipart_transfer.is_some() {
        return Err(GraphicsError::MultipartState);
    }
    let (parameter_bytes, encoded_payload_bytes) = split_bytes_at_delimiter(command_payload, b':')
        .ok_or(GraphicsError::InvalidHeader {
            protocol: ITERM2_PROTOCOL,
        })?;
    let display_options = parse_iterm_display_options(parameter_bytes, true)?;
    let decoded_media_bytes = decode_base64(ITERM2_PROTOCOL, encoded_payload_bytes)?;
    Ok(Some(decode_iterm_graphics(
        display_options,
        &decoded_media_bytes,
    )?))
}

fn parse_multipart_file_command(
    command_payload: &[u8],
    multipart_transfer: &mut Option<ItermTransfer>,
) -> Result<Option<DecodedGraphics>, GraphicsError> {
    if multipart_transfer.is_some() {
        return Err(GraphicsError::MultipartState);
    }
    let (parameter_bytes, encoded_payload_bytes) =
        split_bytes_at_delimiter(command_payload, b':').unwrap_or((command_payload, &[]));
    let display_options = parse_iterm_display_options(parameter_bytes, true)?;
    if encoded_payload_bytes.len() > MAX_GRAPHICS_TRANSFER_BYTE_COUNT {
        return Err(GraphicsError::TransferTooLarge {
            protocol: ITERM2_PROTOCOL,
        });
    }
    *multipart_transfer = Some(ItermTransfer {
        display_options,
        encoded_payload_bytes: encoded_payload_bytes.to_vec(),
    });
    Ok(None)
}

fn parse_file_part_command(
    command_payload: &[u8],
    multipart_transfer: &mut Option<ItermTransfer>,
) -> Result<Option<DecodedGraphics>, GraphicsError> {
    let iterm_transfer = multipart_transfer
        .as_mut()
        .ok_or(GraphicsError::MultipartState)?;
    append_bounded_payload_bytes(&mut iterm_transfer.encoded_payload_bytes, command_payload)?;
    Ok(None)
}

fn parse_file_end_command(
    command_payload: &[u8],
    multipart_transfer: &mut Option<ItermTransfer>,
) -> Result<Option<DecodedGraphics>, GraphicsError> {
    if !command_payload.is_empty() {
        return Err(GraphicsError::InvalidHeader {
            protocol: ITERM2_PROTOCOL,
        });
    }
    let iterm_transfer = multipart_transfer
        .take()
        .ok_or(GraphicsError::MultipartState)?;
    let decoded_media_bytes =
        decode_base64(ITERM2_PROTOCOL, &iterm_transfer.encoded_payload_bytes)?;
    Ok(Some(decode_iterm_graphics(
        iterm_transfer.display_options,
        &decoded_media_bytes,
    )?))
}

fn decode_iterm_graphics(
    display_options: ItermDisplayOptions,
    decoded_media_bytes: &[u8],
) -> Result<DecodedGraphics, GraphicsError> {
    let (decoded_image, animation) = match decode_media(ITERM2_PROTOCOL, decoded_media_bytes)? {
        DecodedMedia::Static(decoded_image) => (decoded_image, None),
        DecodedMedia::Animation(animation) => {
            let first_frame_image = animation.list_frames()[0].get_decoded_image().clone();
            (first_frame_image, Some(animation))
        }
    };
    Ok(DecodedGraphics {
        is_query: false,
        protocol: ITERM2_PROTOCOL,
        image: decoded_image,
        animation,
        action: ImageAction::Display,
        display: display_options.display,
    })
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ItermDisplayOptions {
    display: ImageDisplay,
}

fn parse_iterm_display_options(
    control_bytes: &[u8],
    is_inline_required: bool,
) -> Result<ItermDisplayOptions, GraphicsError> {
    if control_bytes.len() > MAX_GRAPHICS_CONTROL_BYTE_COUNT {
        return Err(GraphicsError::TransferTooLarge {
            protocol: ITERM2_PROTOCOL,
        });
    }
    let mut image_display = ImageDisplay::default();
    let mut is_inline = false;
    for control_field_bytes in control_bytes.split(|byte| *byte == b';') {
        if control_field_bytes.is_empty() {
            continue;
        }
        let (parameter_name, parameter_value) = split_bytes_at_delimiter(control_field_bytes, b'=')
            .ok_or(GraphicsError::InvalidHeader {
                protocol: ITERM2_PROTOCOL,
            })?;
        match parameter_name {
            b"inline" => match parameter_value {
                b"1" => is_inline = true,
                b"0" => is_inline = false,
                _ => {
                    return Err(GraphicsError::InvalidCommand {
                        protocol: ITERM2_PROTOCOL,
                    })
                }
            },
            b"size" => {
                validate_ascii_decimal(parameter_value)?;
            }
            b"width" => {
                image_display.requested_width = Some(parse_iterm_dimension(parameter_value)?);
            }
            b"height" => {
                image_display.requested_height = Some(parse_iterm_dimension(parameter_value)?);
            }
            b"preserveAspectRatio" => match parameter_value {
                b"1" => image_display.is_aspect_ratio_preserved = true,
                b"0" => image_display.is_aspect_ratio_preserved = false,
                _ => {
                    return Err(GraphicsError::InvalidCommand {
                        protocol: ITERM2_PROTOCOL,
                    })
                }
            },
            b"name" => validate_iterm_control_value(parameter_value)?,
            _ => validate_iterm_control_value(parameter_value)?,
        }
    }
    if is_inline_required && !is_inline {
        return Err(GraphicsError::UnsupportedAction {
            protocol: ITERM2_PROTOCOL,
            action: "inline=0".to_string(),
        });
    }
    Ok(ItermDisplayOptions {
        display: image_display,
    })
}

fn validate_iterm_control_value(control_value: &[u8]) -> Result<(), GraphicsError> {
    if control_value.len() > MAX_GRAPHICS_CONTROL_BYTE_COUNT {
        return Err(GraphicsError::TransferTooLarge {
            protocol: ITERM2_PROTOCOL,
        });
    }
    Ok(())
}

fn parse_iterm_dimension(dimension_bytes: &[u8]) -> Result<ImageDimension, GraphicsError> {
    if dimension_bytes == b"auto" {
        return Ok(ImageDimension::Auto);
    }
    if dimension_bytes.ends_with(b"px") {
        return match parse_signed_decimal(
            &dimension_bytes[..dimension_bytes.len().saturating_sub(2)],
        )? {
            SignedDecimal::NonPositive => Ok(ImageDimension::Pixels(1)),
            SignedDecimal::Positive(dimension_value) => Ok(ImageDimension::Pixels(dimension_value)),
            SignedDecimal::TooLarge => Err(build_invalid_dimensions_error()),
        };
    }
    if dimension_bytes.ends_with(b"%") {
        return match parse_signed_decimal(
            &dimension_bytes[..dimension_bytes.len().saturating_sub(1)],
        )? {
            SignedDecimal::NonPositive => Ok(ImageDimension::Cells(1)),
            SignedDecimal::Positive(dimension_value) => Ok(ImageDimension::Percent(
                u16::try_from(dimension_value.min(100)).expect("a clamped percentage fits in u16"),
            )),
            SignedDecimal::TooLarge => Ok(ImageDimension::Percent(100)),
        };
    }
    match parse_signed_decimal(dimension_bytes)? {
        SignedDecimal::NonPositive => Ok(ImageDimension::Cells(1)),
        SignedDecimal::Positive(dimension_value) => Ok(ImageDimension::Cells(dimension_value)),
        SignedDecimal::TooLarge => Err(build_invalid_dimensions_error()),
    }
}

#[derive(Clone, Copy)]
enum SignedDecimal {
    NonPositive,
    Positive(u32),
    TooLarge,
}

fn parse_signed_decimal(decimal_bytes: &[u8]) -> Result<SignedDecimal, GraphicsError> {
    let (is_negative, decimal_digits) = match decimal_bytes {
        [b'-', decimal_digits @ ..] => (true, decimal_digits),
        [b'+', decimal_digits @ ..] => (false, decimal_digits),
        decimal_digits => (false, decimal_digits),
    };
    validate_ascii_decimal(decimal_digits)?;
    let mut decimal_value = 0u32;
    for &decimal_digit in decimal_digits {
        let Some(next_decimal_value) =
            decimal_value
                .checked_mul(10)
                .and_then(|current_decimal_value| {
                    current_decimal_value.checked_add(u32::from(decimal_digit - b'0'))
                })
        else {
            return Ok(if is_negative {
                SignedDecimal::NonPositive
            } else {
                SignedDecimal::TooLarge
            });
        };
        decimal_value = next_decimal_value;
    }
    if is_negative || decimal_value == 0 {
        Ok(SignedDecimal::NonPositive)
    } else {
        Ok(SignedDecimal::Positive(decimal_value))
    }
}

fn validate_ascii_decimal(decimal_bytes: &[u8]) -> Result<(), GraphicsError> {
    if decimal_bytes.is_empty() || !decimal_bytes.iter().all(u8::is_ascii_digit) {
        return Err(GraphicsError::InvalidCommand {
            protocol: ITERM2_PROTOCOL,
        });
    }
    Ok(())
}

fn build_invalid_dimensions_error() -> GraphicsError {
    GraphicsError::InvalidDimensions {
        protocol: ITERM2_PROTOCOL,
    }
}

fn append_bounded_payload_bytes(
    encoded_payload_bytes: &mut Vec<u8>,
    payload_bytes: &[u8],
) -> Result<(), GraphicsError> {
    let encoded_payload_byte_count = encoded_payload_bytes
        .len()
        .checked_add(payload_bytes.len())
        .ok_or(GraphicsError::InvalidDimensions {
            protocol: ITERM2_PROTOCOL,
        })?;
    if encoded_payload_byte_count > MAX_GRAPHICS_TRANSFER_BYTE_COUNT {
        return Err(GraphicsError::TransferTooLarge {
            protocol: ITERM2_PROTOCOL,
        });
    }
    encoded_payload_bytes.extend_from_slice(payload_bytes);
    Ok(())
}

fn split_bytes_at_delimiter(source_bytes: &[u8], delimiter_byte: u8) -> Option<(&[u8], &[u8])> {
    let delimiter_index = source_bytes
        .iter()
        .position(|byte| *byte == delimiter_byte)?;
    Some((
        &source_bytes[..delimiter_index],
        &source_bytes[delimiter_index + 1..],
    ))
}

#[cfg(test)]
mod tests;
