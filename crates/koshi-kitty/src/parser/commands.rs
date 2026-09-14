//! Kitty image-placement and image-deletion command parsing.

use super::parse_kitty_control;
use koshi_image::{GraphicsError, GraphicsProtocol, ImageDisplay, MAX_GRAPHICS_CONTROL_BYTE_COUNT};

/// A Kitty image-placement or image-deletion selector.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KittyDelete {
    /// Delete all visible placements.
    Visible,
    /// Delete placements for an image identifier.
    ImageId,
    /// Delete placements for an image number.
    ImageNumber,
    /// Delete the placement at the cursor.
    Cursor,
    /// Delete placements intersecting one cell.
    Cell,
    /// Delete placements intersecting one cell at one z-index.
    CellAtZ,
    /// Delete an image identifier range.
    ImageIdRange,
    /// Delete placements in a column.
    Column,
    /// Delete placements in a row.
    Row,
    /// Delete placements at one z-index.
    ZIndex,
}

/// The operation requested by a Kitty command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KittyCommandKind {
    /// Place an image.
    Place,
    /// Delete image placements using the selected scope.
    Delete(KittyDelete),
    /// Add or replace one retained animation frame.
    AnimationFrame,
    /// Change retained animation playback state.
    AnimationControl,
    /// Compose one retained frame into another.
    AnimationCompose,
    /// Delete one retained animation frame.
    AnimationDelete,
}

/// The fields carried by one Kitty animation command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KittyAnimationCommand {
    /// The raw frame format: 24 for RGB, 32 for RGBA, or 100 for PNG.
    pub media_format: Option<u32>,
    /// The raw frame width in pixels.
    pub frame_width_pixels: Option<u32>,
    /// The raw frame height in pixels.
    pub frame_height_pixels: Option<u32>,
    /// The target frame number for a frame transfer or control command.
    pub frame_number: Option<u32>,
    /// The frame whose delay is changed by an animation control command.
    pub affected_frame_number: Option<u32>,
    /// The base frame number for a partial frame transfer.
    pub base_frame_number: Option<u32>,
    /// The source frame number for a composition.
    pub source_frame_number: Option<u32>,
    /// The destination frame number for a composition.
    pub destination_frame_number: Option<u32>,
    /// The source or patch x coordinate in pixels.
    pub source_x_pixels: u32,
    /// The source or patch y coordinate in pixels.
    pub source_y_pixels: u32,
    /// The composition destination x coordinate in pixels.
    pub destination_x_pixels: u32,
    /// The composition destination y coordinate in pixels.
    pub destination_y_pixels: u32,
    /// The frame delay in milliseconds, including negative gapless values.
    pub gap_milliseconds: Option<i32>,
    /// The packed RGBA background for a new frame.
    pub background_rgba_bytes: Option<[u8; 4]>,
    /// Whether composition replaces destination pixels instead of blending them.
    pub replaces_destination_pixels: bool,
    /// The playback state requested by a control command.
    pub playback_state: Option<u8>,
    /// The playback loop count requested by a control command.
    pub loop_count: Option<u32>,
    /// The base64 frame payload.
    pub encoded_payload_bytes: Vec<u8>,
}

/// One validated chunk of a multipart Kitty animation-frame transfer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KittyAnimationChunk {
    control_header_bytes: Vec<u8>,
    encoded_payload_bytes: Vec<u8>,
    has_more_chunks: bool,
    is_continuation: bool,
}

impl KittyAnimationChunk {
    /// Return the control bytes, including the leading `G`.
    #[must_use]
    pub(crate) fn get_control_header_bytes(&self) -> &[u8] {
        &self.control_header_bytes
    }

    /// Return the base64 bytes in this chunk.
    #[must_use]
    pub(crate) fn get_encoded_payload_bytes(&self) -> &[u8] {
        &self.encoded_payload_bytes
    }

    /// Return whether another chunk is required.
    #[must_use]
    pub(crate) fn has_more_chunks(&self) -> bool {
        self.has_more_chunks
    }

    /// Return whether this is a continuation chunk.
    #[must_use]
    pub(crate) fn is_continuation(&self) -> bool {
        self.is_continuation
    }
}

/// A validated Kitty image-placement or image-deletion command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KittyCommand {
    command_kind: KittyCommandKind,
    image_display: ImageDisplay,
    should_free_image_data: bool,
    animation_command: Option<KittyAnimationCommand>,
}

impl KittyCommand {
    /// Return the requested Kitty operation.
    #[must_use]
    pub fn get_command_kind(&self) -> KittyCommandKind {
        self.command_kind
    }

    /// Return display and selection metadata carried by the command.
    #[must_use]
    pub fn get_image_display(&self) -> &ImageDisplay {
        &self.image_display
    }

    /// Return whether the command uses the uppercase delete form.
    #[must_use]
    pub fn should_free_image_data(&self) -> bool {
        self.should_free_image_data
    }

    /// Return animation fields for an animation command.
    #[must_use]
    pub fn get_animation_command(&self) -> Option<&KittyAnimationCommand> {
        self.animation_command.as_ref()
    }
}

/// Parse a Kitty placement, deletion, or animation command.
///
/// Returns `None` for other actions and multipart animation-frame starts.
pub fn parse_kitty_command(
    control_header_bytes: &[u8],
    encoded_payload_bytes: &[u8],
) -> Option<Result<KittyCommand, GraphicsError>> {
    let control_header_byte_count = control_header_bytes.len();
    let control_body_bytes = control_header_bytes.strip_prefix(b"G")?;
    let action_code = control_body_bytes
        .split(|byte| *byte == b',')
        .find_map(|control_field| control_field.strip_prefix(b"a="))?;
    let animation_command_kind = match action_code {
        b"f" => Some(KittyCommandKind::AnimationFrame),
        b"a" => Some(KittyCommandKind::AnimationControl),
        b"c" => Some(KittyCommandKind::AnimationCompose),
        b"d" if control_body_bytes
            .split(|byte| *byte == b',')
            .any(|control_field| matches!(control_field, b"d=f" | b"d=F")) =>
        {
            Some(KittyCommandKind::AnimationDelete)
        }
        _ => None,
    };
    if action_code != b"p" && action_code != b"d" && animation_command_kind.is_none() {
        return None;
    }
    if action_code == b"f" && find_control_parameter_bytes(control_body_bytes, b'm') == Some(b"1") {
        return None;
    }
    if control_header_byte_count > MAX_GRAPHICS_CONTROL_BYTE_COUNT {
        return Some(Err(GraphicsError::TransferTooLarge {
            protocol: GraphicsProtocol::Kitty,
        }));
    }
    Some(if let Some(command_kind) = animation_command_kind {
        parse_animation_command_fields(
            control_body_bytes,
            encoded_payload_bytes,
            command_kind,
            false,
        )
    } else {
        parse_kitty_command_fields(
            control_body_bytes,
            encoded_payload_bytes,
            action_code == b"d",
        )
    })
}

/// Parse the first or a continuation chunk of a multipart Kitty animation-frame
/// transfer.
pub(crate) fn parse_animation_transfer_chunk(
    control_header_bytes: &[u8],
    encoded_payload_bytes: &[u8],
) -> Result<Option<KittyAnimationChunk>, GraphicsError> {
    let control_body_bytes =
        control_header_bytes
            .strip_prefix(b"G")
            .ok_or(GraphicsError::InvalidHeader {
                protocol: GraphicsProtocol::Kitty,
            })?;
    let action_code = control_body_bytes
        .split(|byte| *byte == b',')
        .find_map(|control_field| control_field.strip_prefix(b"a="));
    let continuation_fields = control_body_bytes
        .split(|byte| *byte == b',')
        .filter(|control_field| !control_field.is_empty())
        .all(|control_field| {
            control_field.starts_with(b"a=f")
                || control_field.starts_with(b"m=")
                || control_field.starts_with(b"q=")
        });
    if action_code == Some(b"f")
        && continuation_fields
        && find_control_parameter_bytes(control_body_bytes, b'i').is_none()
        && find_control_parameter_bytes(control_body_bytes, b'I').is_none()
    {
        let mut normalized_control_bytes = Vec::new();
        for control_field in control_body_bytes.split(|byte| *byte == b',') {
            if control_field.starts_with(b"a=") {
                normalized_control_bytes.extend_from_slice(b"a=t");
            } else {
                normalized_control_bytes.extend_from_slice(control_field);
            }
            normalized_control_bytes.push(b',');
        }
        normalized_control_bytes.pop();
        let kitty_control = super::parse_kitty_control(&normalized_control_bytes)?;
        if !kitty_control.has_more_chunks_parameter {
            return Err(build_invalid_command_error());
        }
        validate_nonfinal_animation_payload(encoded_payload_bytes, kitty_control.has_more_chunks)?;
        return Ok(Some(KittyAnimationChunk {
            control_header_bytes: control_header_bytes.to_vec(),
            encoded_payload_bytes: encoded_payload_bytes.to_vec(),
            has_more_chunks: kitty_control.has_more_chunks,
            is_continuation: true,
        }));
    }
    if action_code == Some(b"f") {
        if find_control_parameter_bytes(control_body_bytes, b'm') != Some(b"1") {
            return Ok(None);
        }
        parse_animation_command_fields(
            control_body_bytes,
            encoded_payload_bytes,
            KittyCommandKind::AnimationFrame,
            true,
        )?;
        validate_nonfinal_animation_payload(encoded_payload_bytes, true)?;
        return Ok(Some(KittyAnimationChunk {
            control_header_bytes: control_header_bytes.to_vec(),
            encoded_payload_bytes: encoded_payload_bytes.to_vec(),
            has_more_chunks: true,
            is_continuation: false,
        }));
    }
    if action_code.is_some() {
        return Ok(None);
    }
    if control_body_bytes
        .split(|byte| *byte == b',')
        .filter(|control_field| !control_field.is_empty())
        .all(|control_field| control_field.starts_with(b"m=") || control_field.starts_with(b"q="))
    {
        let kitty_control = super::parse_kitty_control(control_body_bytes)?;
        if !kitty_control.has_more_chunks_parameter {
            return Ok(None);
        }
        validate_nonfinal_animation_payload(encoded_payload_bytes, kitty_control.has_more_chunks)?;
        return Ok(Some(KittyAnimationChunk {
            control_header_bytes: control_header_bytes.to_vec(),
            encoded_payload_bytes: encoded_payload_bytes.to_vec(),
            has_more_chunks: kitty_control.has_more_chunks,
            is_continuation: true,
        }));
    }
    Ok(None)
}

fn validate_nonfinal_animation_payload(
    encoded_payload_bytes: &[u8],
    has_more_chunks: bool,
) -> Result<(), GraphicsError> {
    if has_more_chunks
        && (!encoded_payload_bytes.len().is_multiple_of(4) || encoded_payload_bytes.contains(&b'='))
    {
        return Err(GraphicsError::InvalidBase64 {
            protocol: GraphicsProtocol::Kitty,
        });
    }
    Ok(())
}

fn parse_kitty_command_fields(
    control_header_bytes: &[u8],
    encoded_payload_bytes: &[u8],
    is_delete_command: bool,
) -> Result<KittyCommand, GraphicsError> {
    if !encoded_payload_bytes.is_empty() {
        return Err(build_invalid_command_error());
    }
    let mut normalized_control_bytes = Vec::new();
    let mut delete_selector_bytes = None;
    for control_field in control_header_bytes.split(|byte| *byte == b',') {
        if let Some(delete_selector_value_bytes) = control_field.strip_prefix(b"d=") {
            if !is_delete_command
                || delete_selector_bytes
                    .replace(delete_selector_value_bytes)
                    .is_some()
            {
                return Err(build_invalid_command_error());
            }
            continue;
        }
        if !normalized_control_bytes.is_empty() {
            normalized_control_bytes.push(b',');
        }
        if control_field.starts_with(b"a=") {
            normalized_control_bytes.extend_from_slice(b"a=t");
        } else {
            normalized_control_bytes.extend_from_slice(control_field);
        }
    }
    let kitty_control = parse_kitty_control(&normalized_control_bytes)?;
    if kitty_control.has_more_chunks_parameter
        || kitty_control.transfer_medium.is_some()
        || kitty_control.media_format.is_some()
        || kitty_control.is_compressed.is_some()
        || kitty_control.source_byte_count.is_some()
        || kitty_control.source_byte_offset.is_some()
        || kitty_control.image_width_pixels.is_some()
        || kitty_control.image_height_pixels.is_some()
        || (kitty_control.image_display.image_id.is_some()
            && kitty_control.image_display.image_number.is_some())
    {
        return Err(build_invalid_command_error());
    }
    if !is_delete_command {
        return Ok(KittyCommand {
            command_kind: KittyCommandKind::Place,
            image_display: kitty_control.image_display,
            should_free_image_data: false,
            animation_command: None,
        });
    }
    let delete_selector_bytes = delete_selector_bytes.unwrap_or(b"a");
    let [delete_selector_letter] = delete_selector_bytes else {
        return Err(build_invalid_command_error());
    };
    let delete_selector = match delete_selector_letter.to_ascii_lowercase() {
        b'a' => KittyDelete::Visible,
        b'i' => KittyDelete::ImageId,
        b'n' => KittyDelete::ImageNumber,
        b'c' => KittyDelete::Cursor,
        b'p' => KittyDelete::Cell,
        b'q' => KittyDelete::CellAtZ,
        b'r' => KittyDelete::ImageIdRange,
        b'x' => KittyDelete::Column,
        b'y' => KittyDelete::Row,
        b'z' => KittyDelete::ZIndex,
        _ => {
            return Err(GraphicsError::UnsupportedAction {
                protocol: GraphicsProtocol::Kitty,
                action: format!("delete {}", char::from(*delete_selector_letter)),
            })
        }
    };
    let image_display = kitty_control.image_display;
    let has_nonzero_source_pixel_offset_x = image_display
        .source_pixel_offset_x
        .is_some_and(|source_pixel_offset_x| source_pixel_offset_x != 0);
    let has_nonzero_source_pixel_offset_y = image_display
        .source_pixel_offset_y
        .is_some_and(|source_pixel_offset_y| source_pixel_offset_y != 0);
    let is_valid_delete_selection = match delete_selector {
        KittyDelete::ImageId => image_display.image_id.is_some_and(|image_id| image_id != 0),
        KittyDelete::ImageNumber => image_display
            .image_number
            .is_some_and(|image_number| image_number != 0),
        KittyDelete::Cell | KittyDelete::CellAtZ => {
            has_nonzero_source_pixel_offset_x && has_nonzero_source_pixel_offset_y
        }
        KittyDelete::ImageIdRange => {
            has_nonzero_source_pixel_offset_x
                && has_nonzero_source_pixel_offset_y
                && image_display.source_pixel_offset_x <= image_display.source_pixel_offset_y
        }
        KittyDelete::Column => has_nonzero_source_pixel_offset_x,
        KittyDelete::Row => has_nonzero_source_pixel_offset_y,
        KittyDelete::Visible | KittyDelete::Cursor | KittyDelete::ZIndex => true,
    };
    if !is_valid_delete_selection {
        return Err(build_invalid_command_error());
    }
    Ok(KittyCommand {
        command_kind: KittyCommandKind::Delete(delete_selector),
        image_display,
        should_free_image_data: delete_selector_letter.is_ascii_uppercase(),
        animation_command: None,
    })
}

pub(super) fn parse_animation_command_fields(
    control_header_bytes: &[u8],
    encoded_payload_bytes: &[u8],
    command_kind: KittyCommandKind,
    allows_additional_chunks: bool,
) -> Result<KittyCommand, GraphicsError> {
    let mut normalized_control_bytes = Vec::new();
    let mut is_delete_frame = None;
    for control_field in control_header_bytes.split(|byte| *byte == b',') {
        if matches!(control_field, b"d=f" | b"d=F") {
            is_delete_frame = Some(control_field == b"d=F");
            continue;
        }
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
    let mut kitty_control = parse_kitty_control(&normalized_control_bytes)?;
    if (kitty_control.has_more_chunks && !allows_additional_chunks) || kitty_control.is_query {
        return Err(build_invalid_command_error());
    }
    if kitty_control.has_more_chunks
        && kitty_control
            .transfer_medium
            .is_some_and(|transfer_medium| transfer_medium != b'd')
    {
        return Err(build_invalid_command_error());
    }
    if kitty_control.image_display.image_id.is_none()
        && kitty_control.image_display.image_number.is_none()
    {
        return Err(build_invalid_command_error());
    }
    let is_control_key_allowed = |control_key: u8| match command_kind {
        KittyCommandKind::AnimationFrame => {
            matches!(
                control_key,
                b'a' | b'i'
                    | b'I'
                    | b'q'
                    | b'f'
                    | b's'
                    | b'v'
                    | b'x'
                    | b'y'
                    | b'c'
                    | b'r'
                    | b'z'
                    | b'Y'
                    | b'X'
                    | b'm'
                    | b't'
                    | b'o'
                    | b'S'
                    | b'O'
                    | b'N'
            )
        }
        KittyCommandKind::AnimationControl => {
            matches!(
                control_key,
                b'a' | b'i' | b'I' | b'q' | b'c' | b'r' | b's' | b'v' | b'z'
            )
        }
        KittyCommandKind::AnimationCompose => {
            matches!(
                control_key,
                b'a' | b'i'
                    | b'I'
                    | b'q'
                    | b'r'
                    | b'c'
                    | b'x'
                    | b'y'
                    | b'X'
                    | b'Y'
                    | b'w'
                    | b'h'
                    | b'C'
            )
        }
        KittyCommandKind::AnimationDelete => {
            matches!(control_key, b'a' | b'i' | b'I' | b'q' | b'r')
        }
        KittyCommandKind::Place | KittyCommandKind::Delete(_) => false,
    };
    for control_field in control_header_bytes.split(|byte| *byte == b',') {
        let Some(control_key) = control_field.first().copied() else {
            return Err(build_invalid_command_error());
        };
        if control_key != b'd' && !is_control_key_allowed(control_key) {
            return Err(build_invalid_command_error());
        }
    }
    if command_kind == KittyCommandKind::AnimationDelete && is_delete_frame.is_none() {
        return Err(build_invalid_command_error());
    }
    if command_kind != KittyCommandKind::AnimationFrame && !encoded_payload_bytes.is_empty() {
        return Err(build_invalid_command_error());
    }
    let media_format = parse_raw_u32_parameter(control_header_bytes, b'f')?;
    if media_format.is_some_and(|media_format| !matches!(media_format, 24 | 32 | 100)) {
        return Err(GraphicsError::UnsupportedMedia {
            protocol: GraphicsProtocol::Kitty,
            media_format: media_format
                .map_or_else(String::new, |media_format| media_format.to_string()),
        });
    }
    let frame_width_pixels = if command_kind == KittyCommandKind::AnimationCompose {
        parse_raw_u32_parameter(control_header_bytes, b'w')?
    } else {
        parse_raw_u32_parameter(control_header_bytes, b's')?
    };
    let frame_height_pixels = if command_kind == KittyCommandKind::AnimationCompose {
        parse_raw_u32_parameter(control_header_bytes, b'h')?
    } else {
        parse_raw_u32_parameter(control_header_bytes, b'v')?
    };
    if frame_width_pixels == Some(0) || frame_height_pixels == Some(0) {
        return Err(GraphicsError::InvalidDimensions {
            protocol: GraphicsProtocol::Kitty,
        });
    }
    let frame_number = match command_kind {
        KittyCommandKind::AnimationFrame | KittyCommandKind::AnimationDelete => {
            parse_raw_positive_u32_parameter(control_header_bytes, b'r')?
        }
        KittyCommandKind::AnimationControl => {
            parse_raw_positive_u32_parameter(control_header_bytes, b'c')?
        }
        KittyCommandKind::AnimationCompose
        | KittyCommandKind::Place
        | KittyCommandKind::Delete(_) => None,
    };
    let affected_frame_number = (command_kind == KittyCommandKind::AnimationControl)
        .then(|| parse_raw_positive_u32_parameter(control_header_bytes, b'r'))
        .transpose()?
        .flatten();
    let base_frame_number = (command_kind == KittyCommandKind::AnimationFrame)
        .then(|| parse_raw_positive_u32_parameter(control_header_bytes, b'c'))
        .transpose()?
        .flatten();
    let source_frame_number = (command_kind == KittyCommandKind::AnimationCompose)
        .then(|| parse_raw_positive_u32_parameter(control_header_bytes, b'r'))
        .transpose()?
        .flatten();
    let destination_frame_number = (command_kind == KittyCommandKind::AnimationCompose)
        .then(|| parse_raw_positive_u32_parameter(control_header_bytes, b'c'))
        .transpose()?
        .flatten();
    let playback_state = (command_kind == KittyCommandKind::AnimationControl)
        .then(|| parse_raw_u32_parameter(control_header_bytes, b's'))
        .transpose()?
        .flatten()
        .map(|playback_state_value| u8::try_from(playback_state_value).unwrap_or(u8::MAX));
    let loop_count = (command_kind == KittyCommandKind::AnimationControl)
        .then(|| parse_raw_u32_parameter(control_header_bytes, b'v'))
        .transpose()?
        .flatten();
    if playback_state.is_some_and(|control_value| !(1..=3).contains(&control_value)) {
        return Err(build_invalid_command_error());
    }
    let gap_milliseconds = parse_raw_i32_parameter(control_header_bytes, b'z')?
        .filter(|gap_milliseconds| *gap_milliseconds != 0);
    let background_rgba_bytes = (command_kind == KittyCommandKind::AnimationFrame)
        .then(|| parse_raw_u32_parameter(control_header_bytes, b'Y'))
        .transpose()?
        .flatten()
        .map(u32::to_be_bytes);
    let replacement_control_key = if command_kind == KittyCommandKind::AnimationFrame {
        b'X'
    } else {
        b'C'
    };
    let replaces_destination_pixels =
        parse_raw_u32_parameter(control_header_bytes, replacement_control_key)?
            .is_some_and(|replacement_value| replacement_value == 1);
    kitty_control.image_display.requested_width = None;
    kitty_control.image_display.requested_height = None;
    kitty_control.image_display.requested_column_count = None;
    kitty_control.image_display.requested_row_count = None;
    kitty_control.image_display.source_pixel_offset_x = None;
    kitty_control.image_display.source_pixel_offset_y = None;
    kitty_control.image_display.cell_pixel_offset_x = None;
    kitty_control.image_display.cell_pixel_offset_y = None;
    kitty_control.image_display.should_move_cursor = false;
    kitty_control.image_display.z_index = 0;
    let animation_command = KittyAnimationCommand {
        media_format,
        frame_width_pixels,
        frame_height_pixels,
        frame_number,
        affected_frame_number,
        base_frame_number,
        source_frame_number,
        destination_frame_number,
        source_x_pixels: if command_kind == KittyCommandKind::AnimationCompose {
            parse_raw_u32_parameter(control_header_bytes, b'X')?.unwrap_or(0)
        } else {
            parse_raw_u32_parameter(control_header_bytes, b'x')?.unwrap_or(0)
        },
        source_y_pixels: if command_kind == KittyCommandKind::AnimationCompose {
            parse_raw_u32_parameter(control_header_bytes, b'Y')?.unwrap_or(0)
        } else {
            parse_raw_u32_parameter(control_header_bytes, b'y')?.unwrap_or(0)
        },
        destination_x_pixels: if command_kind == KittyCommandKind::AnimationCompose {
            parse_raw_u32_parameter(control_header_bytes, b'x')?.unwrap_or(0)
        } else {
            0
        },
        destination_y_pixels: if command_kind == KittyCommandKind::AnimationCompose {
            parse_raw_u32_parameter(control_header_bytes, b'y')?.unwrap_or(0)
        } else {
            0
        },
        gap_milliseconds,
        background_rgba_bytes,
        replaces_destination_pixels,
        playback_state,
        loop_count,
        encoded_payload_bytes: encoded_payload_bytes.to_vec(),
    };
    Ok(KittyCommand {
        command_kind,
        image_display: kitty_control.image_display,
        should_free_image_data: is_delete_frame.unwrap_or(false),
        animation_command: Some(animation_command),
    })
}

fn find_control_parameter_bytes(control_header_bytes: &[u8], control_key: u8) -> Option<&[u8]> {
    control_header_bytes
        .split(|byte| *byte == b',')
        .find_map(|control_field| {
            (control_field.first().copied() == Some(control_key))
                .then(|| control_field.get(2..))
                .flatten()
        })
}

fn parse_raw_u32_parameter(
    control_header_bytes: &[u8],
    control_key: u8,
) -> Result<Option<u32>, GraphicsError> {
    find_control_parameter_bytes(control_header_bytes, control_key)
        .map(super::parse_decimal_u32)
        .transpose()
}

fn parse_raw_positive_u32_parameter(
    control_header_bytes: &[u8],
    control_key: u8,
) -> Result<Option<u32>, GraphicsError> {
    find_control_parameter_bytes(control_header_bytes, control_key)
        .map(super::parse_positive_decimal_u32)
        .transpose()
}

fn parse_raw_i32_parameter(
    control_header_bytes: &[u8],
    control_key: u8,
) -> Result<Option<i32>, GraphicsError> {
    find_control_parameter_bytes(control_header_bytes, control_key)
        .map(super::parse_decimal_i32)
        .transpose()
}

fn build_invalid_command_error() -> GraphicsError {
    GraphicsError::InvalidCommand {
        protocol: GraphicsProtocol::Kitty,
    }
}

/// Extract Kitty identifiers and response suppression level for an error response.
pub fn parse_reply_display(control_header_bytes: &[u8]) -> ImageDisplay {
    let mut response_display = ImageDisplay::default();
    for control_field in control_header_bytes
        .strip_prefix(b"G")
        .unwrap_or(control_header_bytes)
        .split(|byte| *byte == b',')
    {
        let Some(delimiter_byte_offset) = control_field.iter().position(|byte| *byte == b'=')
        else {
            continue;
        };
        let (control_key, control_value_with_delimiter) =
            control_field.split_at(delimiter_byte_offset);
        let control_value_bytes = &control_value_with_delimiter[1..];
        let Some(control_value_number) = std::str::from_utf8(control_value_bytes)
            .ok()
            .and_then(|control_value_text| control_value_text.parse::<u32>().ok())
        else {
            continue;
        };
        match control_key {
            b"i" => response_display.image_id = Some(control_value_number),
            b"I" => response_display.image_number = Some(control_value_number),
            b"p" => response_display.placement_id = Some(control_value_number),
            b"q" => {
                response_display.response_suppression_level =
                    u8::try_from(control_value_number).unwrap_or(2).min(2)
            }
            _ => {}
        }
    }
    response_display
}

#[cfg(test)]
mod tests;
