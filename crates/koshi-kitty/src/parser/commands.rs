//! Kitty image-placement and image-deletion command parsing.

use super::parse_kitty_control;
use koshi_image::{GraphicsError, GraphicsProtocol, ImageDisplay, MAX_GRAPHICS_CONTROL_BYTES};

/// A Kitty image-placement or image-deletion selector.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KittyDelete {
    /// Delete all visible placements.
    Visible,
    /// Delete placements for an image identifier.
    Id,
    /// Delete placements for an image number.
    Number,
    /// Delete the placement at the cursor.
    Cursor,
    /// Delete placements intersecting one cell.
    Cell,
    /// Delete placements intersecting one cell at one z-index.
    CellAtZ,
    /// Delete an image identifier range.
    IdRange,
    /// Delete placements in a column.
    Column,
    /// Delete placements in a row.
    Row,
    /// Delete placements at one z-index.
    Z,
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
    pub format: Option<u32>,
    /// The raw frame width in pixels.
    pub width: Option<u32>,
    /// The raw frame height in pixels.
    pub height: Option<u32>,
    /// The target frame number for a frame transfer or control command.
    pub frame: Option<u32>,
    /// The frame whose delay is changed by an animation control command.
    pub affected_frame: Option<u32>,
    /// The base frame number for a partial frame transfer.
    pub base_frame: Option<u32>,
    /// The source frame number for a composition.
    pub source_frame: Option<u32>,
    /// The destination frame number for a composition.
    pub destination_frame: Option<u32>,
    /// The source or patch x coordinate in pixels.
    pub source_x: u32,
    /// The source or patch y coordinate in pixels.
    pub source_y: u32,
    /// The composition destination x coordinate in pixels.
    pub destination_x: u32,
    /// The composition destination y coordinate in pixels.
    pub destination_y: u32,
    /// The frame delay in milliseconds, including negative gapless values.
    pub gap_ms: Option<i32>,
    /// The packed RGBA background for a new frame.
    pub background: Option<[u8; 4]>,
    /// Whether composition replaces destination pixels instead of blending them.
    pub replace: bool,
    /// The playback state requested by a control command.
    pub state: Option<u8>,
    /// The playback loop value requested by a control command.
    pub loops: Option<u32>,
    /// The base64 frame payload.
    pub payload: Vec<u8>,
}

/// One validated chunk of a multipart Kitty animation-frame transfer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KittyAnimationChunk {
    header: Vec<u8>,
    payload: Vec<u8>,
    more: bool,
    continuation: bool,
}

impl KittyAnimationChunk {
    /// Return the control bytes, including the leading `G`.
    #[must_use]
    pub(crate) fn header(&self) -> &[u8] {
        &self.header
    }

    /// Return the base64 bytes in this chunk.
    #[must_use]
    pub(crate) fn payload(&self) -> &[u8] {
        &self.payload
    }

    /// Return whether another chunk is required.
    #[must_use]
    pub(crate) fn more(&self) -> bool {
        self.more
    }

    /// Return whether this is a continuation chunk.
    #[must_use]
    pub(crate) fn continuation(&self) -> bool {
        self.continuation
    }
}

/// A validated Kitty image-placement or image-deletion command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KittyCommand {
    kind: KittyCommandKind,
    display: ImageDisplay,
    free_data: bool,
    animation: Option<KittyAnimationCommand>,
}

impl KittyCommand {
    /// Return the requested Kitty operation.
    #[must_use]
    pub fn kind(&self) -> KittyCommandKind {
        self.kind
    }

    /// Return display and selection metadata carried by the command.
    #[must_use]
    pub fn display(&self) -> &ImageDisplay {
        &self.display
    }

    /// Return whether the command uses the uppercase delete form.
    #[must_use]
    pub fn free_data(&self) -> bool {
        self.free_data
    }

    /// Return animation fields for an animation command.
    #[must_use]
    pub fn animation(&self) -> Option<&KittyAnimationCommand> {
        self.animation.as_ref()
    }
}

/// Parse a Kitty command that does not carry image payload bytes.
pub fn parse_command(header: &[u8], payload: &[u8]) -> Option<Result<KittyCommand, GraphicsError>> {
    let header_len = header.len();
    let header = header.strip_prefix(b"G")?;
    let action = header
        .split(|byte| *byte == b',')
        .find_map(|field| field.strip_prefix(b"a="))?;
    let animation = match action {
        b"f" => Some(KittyCommandKind::AnimationFrame),
        b"a" => Some(KittyCommandKind::AnimationControl),
        b"c" => Some(KittyCommandKind::AnimationCompose),
        b"d" if header
            .split(|byte| *byte == b',')
            .any(|field| matches!(field, b"d=f" | b"d=F")) =>
        {
            Some(KittyCommandKind::AnimationDelete)
        }
        _ => None,
    };
    if action != b"p" && action != b"d" && animation.is_none() {
        return None;
    }
    if action == b"f" && raw_value(header, b'm') == Some(b"1") {
        return None;
    }
    if header_len > MAX_GRAPHICS_CONTROL_BYTES {
        return Some(Err(GraphicsError::TransferTooLarge {
            protocol: GraphicsProtocol::Kitty,
        }));
    }
    Some(if let Some(kind) = animation {
        parse_animation_command_fields(header, payload, kind, false)
    } else {
        parse_command_fields(header, payload, action == b"d")
    })
}

/// Parse the first or a continuation chunk of a multipart Kitty animation
/// frame transfer.
pub(crate) fn parse_animation_transfer_chunk(
    header: &[u8],
    payload: &[u8],
) -> Result<Option<KittyAnimationChunk>, GraphicsError> {
    let body = header
        .strip_prefix(b"G")
        .ok_or(GraphicsError::InvalidHeader {
            protocol: GraphicsProtocol::Kitty,
        })?;
    let action = body
        .split(|byte| *byte == b',')
        .find_map(|field| field.strip_prefix(b"a="));
    let continuation_fields = body
        .split(|byte| *byte == b',')
        .filter(|field| !field.is_empty())
        .all(|field| {
            field.starts_with(b"a=f") || field.starts_with(b"m=") || field.starts_with(b"q=")
        });
    if action == Some(b"f")
        && continuation_fields
        && raw_value(header, b'i').is_none()
        && raw_value(header, b'I').is_none()
    {
        let mut normalized = Vec::new();
        for field in body.split(|byte| *byte == b',') {
            if field.starts_with(b"a=") {
                normalized.extend_from_slice(b"a=t");
            } else {
                normalized.extend_from_slice(field);
            }
            normalized.push(b',');
        }
        normalized.pop();
        let control = super::parse_kitty_control(&normalized)?;
        if !control.more_specified {
            return Err(invalid());
        }
        validate_nonfinal_animation_payload(payload, control.more)?;
        return Ok(Some(KittyAnimationChunk {
            header: header.to_vec(),
            payload: payload.to_vec(),
            more: control.more,
            continuation: true,
        }));
    }
    if action == Some(b"f") {
        if raw_value(header, b'm') != Some(b"1") {
            return Ok(None);
        }
        parse_animation_command_fields(body, payload, KittyCommandKind::AnimationFrame, true)?;
        validate_nonfinal_animation_payload(payload, true)?;
        return Ok(Some(KittyAnimationChunk {
            header: header.to_vec(),
            payload: payload.to_vec(),
            more: true,
            continuation: false,
        }));
    }
    if action.is_some() {
        return Ok(None);
    }
    if body
        .split(|byte| *byte == b',')
        .filter(|field| !field.is_empty())
        .all(|field| field.starts_with(b"m=") || field.starts_with(b"q="))
    {
        let control = super::parse_kitty_control(body)?;
        if !control.more_specified {
            return Ok(None);
        }
        validate_nonfinal_animation_payload(payload, control.more)?;
        return Ok(Some(KittyAnimationChunk {
            header: header.to_vec(),
            payload: payload.to_vec(),
            more: control.more,
            continuation: true,
        }));
    }
    Ok(None)
}

fn validate_nonfinal_animation_payload(payload: &[u8], more: bool) -> Result<(), GraphicsError> {
    if more && (!payload.len().is_multiple_of(4) || payload.contains(&b'=')) {
        return Err(GraphicsError::InvalidBase64 {
            protocol: GraphicsProtocol::Kitty,
        });
    }
    Ok(())
}

fn parse_command_fields(
    header: &[u8],
    payload: &[u8],
    delete: bool,
) -> Result<KittyCommand, GraphicsError> {
    if !payload.is_empty() {
        return Err(invalid());
    }
    let mut fields = Vec::new();
    let mut selector = None;
    for field in header.split(|byte| *byte == b',') {
        if let Some(value) = field.strip_prefix(b"d=") {
            if !delete || selector.replace(value).is_some() {
                return Err(invalid());
            }
        } else {
            if !fields.is_empty() {
                fields.push(b',');
            }
            if field.starts_with(b"a=") {
                fields.extend_from_slice(b"a=t");
            } else {
                fields.extend_from_slice(field);
            }
        }
    }
    let control = parse_kitty_control(&fields)?;
    if control.more_specified
        || control.medium.is_some()
        || control.format.is_some()
        || control.compression.is_some()
        || control.source_size.is_some()
        || control.source_offset.is_some()
        || control.width.is_some()
        || control.height.is_some()
        || (control.display.image_id.is_some() && control.display.image_number.is_some())
    {
        return Err(invalid());
    }
    if !delete {
        return Ok(KittyCommand {
            kind: KittyCommandKind::Place,
            display: control.display,
            free_data: false,
            animation: None,
        });
    }
    let selector = selector.unwrap_or(b"a");
    let [letter] = selector else {
        return Err(invalid());
    };
    let kind = match letter.to_ascii_lowercase() {
        b'a' => KittyDelete::Visible,
        b'i' => KittyDelete::Id,
        b'n' => KittyDelete::Number,
        b'c' => KittyDelete::Cursor,
        b'p' => KittyDelete::Cell,
        b'q' => KittyDelete::CellAtZ,
        b'r' => KittyDelete::IdRange,
        b'x' => KittyDelete::Column,
        b'y' => KittyDelete::Row,
        b'z' => KittyDelete::Z,
        _ => {
            return Err(GraphicsError::UnsupportedAction {
                protocol: GraphicsProtocol::Kitty,
                action: format!("delete {}", char::from(*letter)),
            })
        }
    };
    let display = control.display;
    let has_x = display.source_offset_x.is_some_and(|value| value != 0);
    let has_y = display.source_offset_y.is_some_and(|value| value != 0);
    let valid = match kind {
        KittyDelete::Id => display.image_id.is_some_and(|id| id != 0),
        KittyDelete::Number => display.image_number.is_some_and(|id| id != 0),
        KittyDelete::Cell | KittyDelete::CellAtZ => has_x && has_y,
        KittyDelete::IdRange => {
            has_x && has_y && display.source_offset_x <= display.source_offset_y
        }
        KittyDelete::Column => has_x,
        KittyDelete::Row => has_y,
        KittyDelete::Visible | KittyDelete::Cursor | KittyDelete::Z => true,
    };
    if !valid {
        return Err(invalid());
    }
    Ok(KittyCommand {
        kind: KittyCommandKind::Delete(kind),
        display,
        free_data: letter.is_ascii_uppercase(),
        animation: None,
    })
}

pub(super) fn parse_animation_command_fields(
    header: &[u8],
    payload: &[u8],
    kind: KittyCommandKind,
    allow_more: bool,
) -> Result<KittyCommand, GraphicsError> {
    let mut normalized = Vec::new();
    let mut delete_frame = None;
    for field in header.split(|byte| *byte == b',') {
        if matches!(field, b"d=f" | b"d=F") {
            delete_frame = Some(field == b"d=F");
            continue;
        }
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
    let mut control = parse_kitty_control(&normalized)?;
    if (control.more && !allow_more) || control.query {
        return Err(invalid());
    }
    if control.more && control.medium.is_some_and(|medium| medium != b'd') {
        return Err(invalid());
    }
    if control.display.image_id.is_none() && control.display.image_number.is_none() {
        return Err(invalid());
    }
    let allowed = |key: u8| match kind {
        KittyCommandKind::AnimationFrame => {
            matches!(
                key,
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
                key,
                b'a' | b'i' | b'I' | b'q' | b'c' | b'r' | b's' | b'v' | b'z'
            )
        }
        KittyCommandKind::AnimationCompose => {
            matches!(
                key,
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
        KittyCommandKind::AnimationDelete => matches!(key, b'a' | b'i' | b'I' | b'q' | b'r'),
        KittyCommandKind::Place | KittyCommandKind::Delete(_) => false,
    };
    for field in header.split(|byte| *byte == b',') {
        let Some(key) = field.first().copied() else {
            return Err(invalid());
        };
        if key != b'd' && !allowed(key) {
            return Err(invalid());
        }
    }
    if kind == KittyCommandKind::AnimationDelete && delete_frame.is_none() {
        return Err(invalid());
    }
    if kind != KittyCommandKind::AnimationFrame && !payload.is_empty() {
        return Err(invalid());
    }
    let format = raw_u32(header, b'f')?;
    if format.is_some_and(|format| !matches!(format, 24 | 32 | 100)) {
        return Err(GraphicsError::UnsupportedMedia {
            protocol: GraphicsProtocol::Kitty,
            format: format.map_or_else(String::new, |format| format.to_string()),
        });
    }
    let width = if kind == KittyCommandKind::AnimationCompose {
        raw_u32(header, b'w')?
    } else {
        raw_u32(header, b's')?
    };
    let height = if kind == KittyCommandKind::AnimationCompose {
        raw_u32(header, b'h')?
    } else {
        raw_u32(header, b'v')?
    };
    if width == Some(0) || height == Some(0) {
        return Err(GraphicsError::InvalidDimensions {
            protocol: GraphicsProtocol::Kitty,
        });
    }
    let frame = match kind {
        KittyCommandKind::AnimationFrame | KittyCommandKind::AnimationDelete => {
            raw_positive_u32(header, b'r')?
        }
        KittyCommandKind::AnimationControl => raw_positive_u32(header, b'c')?,
        KittyCommandKind::AnimationCompose
        | KittyCommandKind::Place
        | KittyCommandKind::Delete(_) => None,
    };
    let affected_frame = (kind == KittyCommandKind::AnimationControl)
        .then(|| raw_positive_u32(header, b'r'))
        .transpose()?
        .flatten();
    let base_frame = (kind == KittyCommandKind::AnimationFrame)
        .then(|| raw_positive_u32(header, b'c'))
        .transpose()?
        .flatten();
    let source_frame = (kind == KittyCommandKind::AnimationCompose)
        .then(|| raw_positive_u32(header, b'r'))
        .transpose()?
        .flatten();
    let destination_frame = (kind == KittyCommandKind::AnimationCompose)
        .then(|| raw_positive_u32(header, b'c'))
        .transpose()?
        .flatten();
    let state = (kind == KittyCommandKind::AnimationControl)
        .then(|| raw_u32(header, b's'))
        .transpose()?
        .flatten()
        .map(|value| u8::try_from(value).unwrap_or(u8::MAX));
    let loops = (kind == KittyCommandKind::AnimationControl)
        .then(|| raw_u32(header, b'v'))
        .transpose()?
        .flatten();
    if state.is_some_and(|value| !(1..=3).contains(&value)) {
        return Err(invalid());
    }
    let gap_ms = raw_i32(header, b'z')?.filter(|value| *value != 0);
    let background = (kind == KittyCommandKind::AnimationFrame)
        .then(|| raw_u32(header, b'Y'))
        .transpose()?
        .flatten()
        .map(u32::to_be_bytes);
    let replace_key = if kind == KittyCommandKind::AnimationFrame {
        b'X'
    } else {
        b'C'
    };
    let replace = raw_u32(header, replace_key)?.is_some_and(|value| value == 1);
    control.display.width = None;
    control.display.height = None;
    control.display.cell_columns = None;
    control.display.cell_rows = None;
    control.display.source_offset_x = None;
    control.display.source_offset_y = None;
    control.display.cell_offset_x = None;
    control.display.cell_offset_y = None;
    control.display.move_cursor = false;
    control.display.z_index = 0;
    let animation = KittyAnimationCommand {
        format,
        width,
        height,
        frame,
        affected_frame,
        base_frame,
        source_frame,
        destination_frame,
        source_x: if kind == KittyCommandKind::AnimationCompose {
            raw_u32(header, b'X')?.unwrap_or(0)
        } else {
            raw_u32(header, b'x')?.unwrap_or(0)
        },
        source_y: if kind == KittyCommandKind::AnimationCompose {
            raw_u32(header, b'Y')?.unwrap_or(0)
        } else {
            raw_u32(header, b'y')?.unwrap_or(0)
        },
        destination_x: if kind == KittyCommandKind::AnimationCompose {
            raw_u32(header, b'x')?.unwrap_or(0)
        } else {
            0
        },
        destination_y: if kind == KittyCommandKind::AnimationCompose {
            raw_u32(header, b'y')?.unwrap_or(0)
        } else {
            0
        },
        gap_ms,
        background,
        replace,
        state,
        loops,
        payload: payload.to_vec(),
    };
    Ok(KittyCommand {
        kind,
        display: control.display,
        free_data: delete_frame.unwrap_or(false),
        animation: Some(animation),
    })
}

fn raw_value(header: &[u8], key: u8) -> Option<&[u8]> {
    header.split(|byte| *byte == b',').find_map(|field| {
        (field.first().copied() == Some(key))
            .then(|| field.get(2..))
            .flatten()
    })
}

fn raw_u32(header: &[u8], key: u8) -> Result<Option<u32>, GraphicsError> {
    raw_value(header, key).map(super::parse_u32).transpose()
}

fn raw_positive_u32(header: &[u8], key: u8) -> Result<Option<u32>, GraphicsError> {
    raw_value(header, key)
        .map(super::parse_positive_u32)
        .transpose()
}

fn raw_i32(header: &[u8], key: u8) -> Result<Option<i32>, GraphicsError> {
    raw_value(header, key).map(super::parse_i32).transpose()
}

fn invalid() -> GraphicsError {
    GraphicsError::InvalidCommand {
        protocol: GraphicsProtocol::Kitty,
    }
}

/// Extract Kitty identifiers and quiet level for an error response.
pub fn reply_display(header: &[u8]) -> ImageDisplay {
    let mut display = ImageDisplay::default();
    for field in header
        .strip_prefix(b"G")
        .unwrap_or(header)
        .split(|byte| *byte == b',')
    {
        let Some(index) = field.iter().position(|byte| *byte == b'=') else {
            continue;
        };
        let (key, tail) = field.split_at(index);
        let value = &tail[1..];
        let Some(value) = std::str::from_utf8(value)
            .ok()
            .and_then(|value| value.parse::<u32>().ok())
        else {
            continue;
        };
        match key {
            b"i" => display.image_id = Some(value),
            b"I" => display.image_number = Some(value),
            b"p" => display.placement_id = Some(value),
            b"q" => display.quiet = u8::try_from(value).unwrap_or(2).min(2),
            _ => {}
        }
    }
    display
}

#[cfg(test)]
mod tests;
