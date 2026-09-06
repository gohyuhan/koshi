//! Incremental decoding of DEC Sixel data.

use std::fmt;

use koshi_image::{
    checked_rgba_len, validate_dimensions, DecodedImage, GraphicsError, GraphicsProtocol,
    SixelBackground, MAX_GRAPHICS_CONTROL_BYTES, MAX_GRAPHICS_TRANSFER_BYTES, MAX_IMAGE_PIXELS,
    MAX_IMAGE_SIDE,
};
use serde::de::{self, SeqAccess, Visitor};
use serde::ser::SerializeSeq;
use serde::{Deserialize, Deserializer, Serialize};

const SIXEL_REGISTER_COUNT: usize = 256;
const SIXEL_UNTOUCHED: u16 = SIXEL_REGISTER_COUNT as u16;
const SIXEL_TERMINAL_BACKGROUND: u16 = SIXEL_UNTOUCHED + 1;
const SIXEL_MAX_INDEX: u16 = SIXEL_TERMINAL_BACKGROUND;

#[cfg(test)]
mod tests;

/// The part of a Sixel string currently being decoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SixelPhase {
    /// The parser is collecting the DCS parameters before `q`.
    Header,
    /// The parser is decoding Sixel commands and data.
    Body,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SixelCommand {
    Repeat,
    Raster,
    Color,
}

/// The indexed result of one Sixel DCS payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SixelGraphic {
    image: Option<IndexedImage>,
    palette_changes: SixelPaletteChanges,
    background: SixelBackground,
}

impl SixelGraphic {
    /// Return the indexed image, or `None` when the payload has no drawable extent.
    #[must_use]
    pub fn image(&self) -> Option<&IndexedImage> {
        self.image.as_ref()
    }

    /// Return the palette edits made by this payload.
    #[must_use]
    pub fn palette_changes(&self) -> &SixelPaletteChanges {
        &self.palette_changes
    }

    /// Return the payload's zero-bit background rule.
    #[must_use]
    pub fn background(&self) -> SixelBackground {
        self.background
    }
}

/// A bounded sequence of Sixel palette register edits.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct SixelPaletteChanges {
    entries: Vec<SixelPaletteChange>,
}

impl SixelPaletteChanges {
    /// Return edits in their first-seen register order.
    #[must_use]
    pub fn entries(&self) -> &[SixelPaletteChange] {
        &self.entries
    }

    fn set(&mut self, register: u8, color: [u8; 3]) -> Result<(), GraphicsError> {
        if let Some(entry) = self
            .entries
            .iter_mut()
            .find(|entry| entry.register == register)
        {
            entry.color = color;
            return Ok(());
        }
        if self.entries.len() == SIXEL_REGISTER_COUNT {
            return Err(image_too_large());
        }
        self.entries.try_reserve(1).map_err(|_| decode_failure())?;
        self.entries.push(SixelPaletteChange { register, color });
        Ok(())
    }
}

impl<'de> Deserialize<'de> for SixelPaletteChanges {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct ChangesVisitor;

        impl<'de> Visitor<'de> for ChangesVisitor {
            type Value = SixelPaletteChanges;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a bounded sequence of unique Sixel palette changes")
            }

            fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
            where
                A: SeqAccess<'de>,
            {
                let mut entries = Vec::new();
                while let Some(entry) = sequence.next_element::<SixelPaletteChange>()? {
                    if entries.len() == SIXEL_REGISTER_COUNT {
                        return Err(de::Error::custom(
                            "Sixel palette changes exceed 256 registers",
                        ));
                    }
                    if entries
                        .iter()
                        .any(|existing: &SixelPaletteChange| existing.register == entry.register)
                    {
                        return Err(de::Error::custom(
                            "Sixel palette changes contain a duplicate register",
                        ));
                    }
                    entries.push(entry);
                }
                Ok(SixelPaletteChanges { entries })
            }
        }

        deserializer.deserialize_seq(ChangesVisitor)
    }
}

/// One Sixel palette register edit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SixelPaletteChange {
    register: u8,
    color: [u8; 3],
}

impl SixelPaletteChange {
    /// Return the edited register number.
    #[must_use]
    pub fn register(&self) -> u8 {
        self.register
    }

    /// Return the edited RGB color.
    #[must_use]
    pub fn color(&self) -> [u8; 3] {
        self.color
    }
}

/// The 256 actual Sixel RGB registers used when resolving an indexed image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SixelPalette {
    colors: [[u8; 3]; SIXEL_REGISTER_COUNT],
}

impl Default for SixelPalette {
    fn default() -> Self {
        let mut colors = [[0, 0, 0]; SIXEL_REGISTER_COUNT];
        let defaults = [
            (0, 0, 0),
            (20, 20, 80),
            (80, 13, 13),
            (20, 80, 20),
            (80, 20, 80),
            (20, 80, 80),
            (80, 80, 20),
            (53, 53, 53),
            (26, 26, 26),
            (33, 33, 60),
            (60, 26, 26),
            (33, 60, 33),
            (60, 33, 60),
            (33, 60, 60),
            (60, 60, 33),
            (80, 80, 80),
        ];
        for (index, (red, green, blue)) in defaults.into_iter().enumerate() {
            colors[index] = [
                percentage_to_byte(red),
                percentage_to_byte(green),
                percentage_to_byte(blue),
            ];
        }
        SixelPalette { colors }
    }
}

impl SixelPalette {
    /// Return one actual RGB register color.
    #[must_use]
    pub fn color(&self, register: u8) -> [u8; 3] {
        self.colors[usize::from(register)]
    }

    /// Apply a payload's register edits to this palette.
    pub fn apply_changes(&mut self, changes: &SixelPaletteChanges) {
        for entry in &changes.entries {
            self.colors[usize::from(entry.register)] = entry.color;
        }
    }
}

impl Serialize for SixelPalette {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let mut sequence = serializer.serialize_seq(Some(SIXEL_REGISTER_COUNT))?;
        for color in &self.colors {
            sequence.serialize_element(color)?;
        }
        sequence.end()
    }
}

impl<'de> Deserialize<'de> for SixelPalette {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct PaletteVisitor;

        impl<'de> Visitor<'de> for PaletteVisitor {
            type Value = SixelPalette;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("exactly 256 Sixel RGB palette colors")
            }

            fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
            where
                A: SeqAccess<'de>,
            {
                let mut colors = [[0, 0, 0]; SIXEL_REGISTER_COUNT];
                for color in &mut colors {
                    *color = sequence.next_element()?.ok_or_else(|| {
                        de::Error::custom("Sixel palette has fewer than 256 colors")
                    })?;
                }
                if sequence.next_element::<de::IgnoredAny>()?.is_some() {
                    return Err(de::Error::custom("Sixel palette has more than 256 colors"));
                }
                Ok(SixelPalette { colors })
            }
        }

        deserializer.deserialize_seq(PaletteVisitor)
    }
}

/// A bounded row-major Sixel register-index image before palette resolution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct IndexedImage {
    width: u32,
    height: u32,
    indices: Vec<u16>,
    aspect_vertical: u32,
    aspect_horizontal: u32,
}

impl IndexedImage {
    /// Return the raw indexed width.
    #[must_use]
    pub fn width(&self) -> u32 {
        self.width
    }

    /// Return the raw indexed height.
    #[must_use]
    pub fn height(&self) -> u32 {
        self.height
    }

    /// Return the normalized `(vertical, horizontal)` pixel aspect ratio.
    #[must_use]
    pub fn pixel_aspect(&self) -> (u32, u32) {
        (self.aspect_vertical, self.aspect_horizontal)
    }

    /// Resolve register indices, background cells, and pixel aspect into RGBA.
    pub fn resolve(
        &self,
        palette: &SixelPalette,
        background: [u8; 3],
    ) -> Result<DecodedImage, GraphicsError> {
        let width = usize::try_from(self.width).map_err(|_| image_too_large())?;
        let height = usize::try_from(self.height).map_err(|_| image_too_large())?;
        let (output_width, output_height, aspect_vertical, aspect_horizontal) =
            expanded_dimensions(width, height, self.aspect_vertical, self.aspect_horizontal)?;
        let rgba_len = checked_rgba_len(GraphicsProtocol::Sixel, output_width, output_height)?;
        let mut rgba = Vec::new();
        rgba.try_reserve_exact(rgba_len)
            .map_err(|_| decode_failure())?;
        rgba.resize(rgba_len, 0);

        for raw_y in 0..height {
            for raw_x in 0..width {
                let index = self.indices[raw_y * width + raw_x];
                let pixel = match index {
                    0..=255 => {
                        let [red, green, blue] = palette.colors[usize::from(index)];
                        [red, green, blue, 255]
                    }
                    SIXEL_UNTOUCHED => [0, 0, 0, 0],
                    SIXEL_TERMINAL_BACKGROUND => [background[0], background[1], background[2], 255],
                    _ => return Err(decode_failure()),
                };
                let first_y = raw_y * aspect_vertical;
                let first_x = raw_x * aspect_horizontal;
                for expanded_y in first_y..first_y + aspect_vertical {
                    let row_start = (expanded_y * output_width + first_x) * 4;
                    for expanded_x in 0..aspect_horizontal {
                        let at = row_start + expanded_x * 4;
                        rgba[at..at + 4].copy_from_slice(&pixel);
                    }
                }
            }
        }

        Ok(DecodedImage {
            width: u32::try_from(output_width).map_err(|_| image_too_large())?,
            height: u32::try_from(output_height).map_err(|_| image_too_large())?,
            rgba,
        })
    }

    fn from_parts(
        width: u32,
        height: u32,
        indices: Vec<u16>,
        aspect_vertical: u32,
        aspect_horizontal: u32,
    ) -> Result<Self, GraphicsError> {
        let width_usize = usize::try_from(width).map_err(|_| image_too_large())?;
        let height_usize = usize::try_from(height).map_err(|_| image_too_large())?;
        validate_dimensions(GraphicsProtocol::Sixel, width_usize, height_usize)?;
        if aspect_vertical == 0
            || aspect_horizontal == 0
            || gcd(aspect_vertical, aspect_horizontal) != 1
        {
            return Err(invalid_dimensions());
        }
        let expected = width_usize
            .checked_mul(height_usize)
            .ok_or_else(invalid_dimensions)?;
        if indices.len() != expected || indices.iter().copied().any(|index| index > SIXEL_MAX_INDEX)
        {
            return Err(invalid_dimensions());
        }
        expanded_dimensions(
            width_usize,
            height_usize,
            aspect_vertical,
            aspect_horizontal,
        )?;
        Ok(IndexedImage {
            width,
            height,
            indices,
            aspect_vertical,
            aspect_horizontal,
        })
    }
}

impl<'de> Deserialize<'de> for IndexedImage {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct IndexedImageFields {
            width: u32,
            height: u32,
            indices: BoundedIndices,
            aspect_vertical: u32,
            aspect_horizontal: u32,
        }

        let fields = IndexedImageFields::deserialize(deserializer)?;
        IndexedImage::from_parts(
            fields.width,
            fields.height,
            fields.indices.0,
            fields.aspect_vertical,
            fields.aspect_horizontal,
        )
        .map_err(|error| de::Error::custom(error.to_string()))
    }
}

struct BoundedIndices(Vec<u16>);

impl<'de> Deserialize<'de> for BoundedIndices {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct IndicesVisitor;

        impl<'de> Visitor<'de> for IndicesVisitor {
            type Value = BoundedIndices;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a bounded Sixel index sequence")
            }

            fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
            where
                A: SeqAccess<'de>,
            {
                let mut indices = Vec::new();
                while let Some(index) = sequence.next_element::<u16>()? {
                    if index > SIXEL_MAX_INDEX {
                        return Err(de::Error::custom(
                            "Sixel indexed pixels contain an invalid register or sentinel",
                        ));
                    }
                    if indices.len() == MAX_IMAGE_PIXELS {
                        return Err(de::Error::custom(
                            "Sixel indexed pixels exceed the image pixel limit",
                        ));
                    }
                    if indices.len() == indices.capacity() {
                        if indices.capacity() >= MAX_IMAGE_PIXELS {
                            return Err(de::Error::custom(
                                "Sixel indexed pixels exceed the image pixel limit",
                            ));
                        }
                        let target_capacity = indices
                            .capacity()
                            .saturating_mul(2)
                            .clamp(1, MAX_IMAGE_PIXELS);
                        indices
                            .try_reserve_exact(target_capacity - indices.len())
                            .map_err(|_| {
                                de::Error::custom("Sixel indexed pixels cannot be allocated")
                            })?;
                    }
                    indices.push(index);
                }
                Ok(BoundedIndices(indices))
            }
        }

        deserializer.deserialize_seq(IndicesVisitor)
    }
}

/// Incremental decoder for one Sixel DCS payload.
///
/// The owning terminal parser handles DCS opening and termination. It passes
/// the DCS parameters, `q`, and body bytes to this type, without the string
/// terminator. The public `phase` and `escaped` fields are the small bridge
/// needed by that parser while it carries a split DCS across input chunks.
#[derive(Debug, Clone)]
pub struct SixelParser {
    /// Whether the parser is collecting header parameters or body data.
    pub phase: SixelPhase,
    header: Vec<u8>,
    command: Option<SixelCommand>,
    command_data: Vec<u8>,
    canvas: SixelCanvas,
    /// Whether the owning DCS parser has just received `ESC`.
    pub escaped: bool,
    input_bytes: usize,
}

impl SixelParser {
    /// Create an empty Sixel payload parser.
    #[must_use]
    pub fn new() -> Self {
        SixelParser {
            phase: SixelPhase::Header,
            header: Vec::new(),
            command: None,
            command_data: Vec::new(),
            canvas: SixelCanvas::new(),
            escaped: false,
            input_bytes: 0,
        }
    }

    /// Feed one byte between the Sixel `q` introducer and DCS terminator.
    pub fn feed(&mut self, byte: u8) -> Result<(), GraphicsError> {
        if self.input_bytes == MAX_GRAPHICS_TRANSFER_BYTES {
            return Err(transfer_too_large());
        }
        self.input_bytes += 1;
        if byte == 0 || byte.is_ascii_whitespace() {
            return Ok(());
        }

        if self.phase == SixelPhase::Header {
            if byte == b'q' {
                self.parse_header()?;
                self.phase = SixelPhase::Body;
            } else if byte.is_ascii_digit() || byte == b';' {
                push_bounded(&mut self.header, byte)?;
            } else {
                return Err(invalid_header());
            }
            return Ok(());
        }

        if let Some(command) = self.command {
            if byte.is_ascii_digit() || byte == b';' {
                push_bounded(&mut self.command_data, byte)?;
                return Ok(());
            }
            self.finish_command(command)?;
        }

        if self.canvas.repeat_pending && !(b'?'..=b'~').contains(&byte) {
            return Err(invalid_command());
        }

        match byte {
            b'!' => {
                self.command = Some(SixelCommand::Repeat);
                self.command_data.clear();
            }
            b'"' => {
                if self.canvas.data_started {
                    return Err(invalid_command());
                }
                self.command = Some(SixelCommand::Raster);
                self.command_data.clear();
            }
            b'#' => {
                self.command = Some(SixelCommand::Color);
                self.command_data.clear();
            }
            b'$' => self.canvas.carriage_return(),
            b'-' => self.canvas.new_line()?,
            b'?'..=b'~' => self.canvas.paint(byte - b'?')?,
            _ => return Err(invalid_command()),
        }
        Ok(())
    }

    /// Finish the payload and return its indexed image and protocol metadata.
    pub fn finish(mut self) -> Result<SixelGraphic, GraphicsError> {
        if let Some(command) = self.command.take() {
            self.finish_command(command)?;
        }
        if self.canvas.repeat_pending || self.phase != SixelPhase::Body {
            return Err(invalid_command());
        }
        Ok(SixelGraphic {
            image: self.canvas.finish()?,
            palette_changes: self.canvas.palette_changes,
            background: self.canvas.background,
        })
    }

    fn parse_header(&mut self) -> Result<(), GraphicsError> {
        let params = parse_sixel_header_params(&self.header, 3)?;
        let aspect = params.first().copied().unwrap_or(0);
        if aspect > 9 {
            return Err(invalid_command());
        }
        match params.get(1).copied().unwrap_or(0) {
            0 | 2 => self.canvas.background = SixelBackground::Terminal,
            1 => self.canvas.background = SixelBackground::Preserve,
            _ => return Err(invalid_command()),
        }
        self.canvas.set_macro_aspect(aspect);
        Ok(())
    }

    fn finish_command(&mut self, command: SixelCommand) -> Result<(), GraphicsError> {
        let data = std::mem::take(&mut self.command_data);
        self.command = None;
        match command {
            SixelCommand::Repeat => {
                let count = parse_repeat_count(&data)?;
                self.canvas.set_repeat(count)?;
            }
            SixelCommand::Raster => {
                let params = parse_sixel_params(&data, 4)?;
                self.canvas.set_raster(
                    params.first().copied().unwrap_or(0),
                    params.get(1).copied().unwrap_or(0),
                    params.get(2).copied().unwrap_or(0),
                    params.get(3).copied().unwrap_or(0),
                )?;
            }
            SixelCommand::Color => self.canvas.set_color(&data)?,
        }
        Ok(())
    }
}

impl Default for SixelParser {
    fn default() -> Self {
        Self::new()
    }
}

/// The bounded indexed canvas used while one Sixel payload is decoded.
#[derive(Debug, Clone)]
struct SixelCanvas {
    width: usize,
    height: usize,
    indices: Vec<u16>,
    background: SixelBackground,
    palette_changes: SixelPaletteChanges,
    color: u16,
    x: usize,
    y: usize,
    repeat: usize,
    repeat_pending: bool,
    data_started: bool,
    aspect_vertical: u32,
    aspect_horizontal: u32,
    declared_width: Option<usize>,
    declared_height: Option<usize>,
    written_width: usize,
    written_height: usize,
    set_width: usize,
    set_height: usize,
}

impl SixelCanvas {
    fn new() -> Self {
        SixelCanvas {
            width: 0,
            height: 0,
            indices: Vec::new(),
            background: SixelBackground::Terminal,
            palette_changes: SixelPaletteChanges::default(),
            color: 0,
            x: 0,
            y: 0,
            repeat: 1,
            repeat_pending: false,
            data_started: false,
            aspect_vertical: 2,
            aspect_horizontal: 1,
            declared_width: None,
            declared_height: None,
            written_width: 0,
            written_height: 0,
            set_width: 0,
            set_height: 0,
        }
    }

    fn set_macro_aspect(&mut self, macro_parameter: u32) {
        let (vertical, horizontal) = match macro_parameter {
            2 => (5, 1),
            3 | 4 => (3, 1),
            5 | 6 => (2, 1),
            7..=9 => (1, 1),
            _ => (2, 1),
        };
        self.aspect_vertical = vertical;
        self.aspect_horizontal = horizontal;
    }

    fn set_repeat(&mut self, count: u32) -> Result<(), GraphicsError> {
        let repeat = if count == 0 { 1 } else { count };
        let repeat = usize::try_from(repeat).map_err(|_| image_too_large())?;
        if repeat > MAX_IMAGE_SIDE {
            return Err(image_too_large());
        }
        self.repeat = repeat;
        self.repeat_pending = true;
        Ok(())
    }

    fn set_raster(
        &mut self,
        pan: u32,
        pad: u32,
        width: u32,
        height: u32,
    ) -> Result<(), GraphicsError> {
        if self.data_started {
            return Err(invalid_command());
        }
        let pan = if pan == 0 { 1 } else { pan };
        let pad = if pad == 0 { 1 } else { pad };
        let divisor = gcd(pan, pad);
        self.aspect_vertical = pan / divisor;
        self.aspect_horizontal = pad / divisor;
        self.declared_width = parse_declared_axis(width)?;
        self.declared_height = parse_declared_axis(height)?;
        Ok(())
    }

    fn set_color(&mut self, data: &[u8]) -> Result<(), GraphicsError> {
        let params = parse_sixel_params(data, 5)?;
        if params.is_empty() || params[0] > 255 {
            return Err(invalid_command());
        }
        let register = u8::try_from(params[0]).map_err(|_| invalid_command())?;
        match params.len() {
            1 => {
                self.color = u16::from(register);
            }
            5 => {
                let color = match params[1] {
                    1 if params[2] <= 360 && params[3] <= 100 && params[4] <= 100 => {
                        hls_to_rgb(params[2], params[3], params[4])
                    }
                    2 if params[2] <= 100 && params[3] <= 100 && params[4] <= 100 => [
                        percentage_to_byte(params[2]),
                        percentage_to_byte(params[3]),
                        percentage_to_byte(params[4]),
                    ],
                    _ => return Err(invalid_command()),
                };
                self.palette_changes.set(register, color)?;
                self.color = u16::from(register);
            }
            _ => return Err(invalid_command()),
        }
        Ok(())
    }

    fn paint(&mut self, bits: u8) -> Result<(), GraphicsError> {
        let repeat = self.repeat;
        self.repeat = 1;
        self.repeat_pending = false;
        let end_x = self.x.checked_add(repeat).ok_or_else(invalid_dimensions)?;
        if end_x > MAX_IMAGE_SIDE {
            return Err(image_too_large());
        }
        let end_y = self.y.checked_add(6).ok_or_else(invalid_dimensions)?;
        if self.background == SixelBackground::Terminal && end_y > MAX_IMAGE_SIDE {
            return Err(image_too_large());
        }
        self.data_started = true;

        if self.background == SixelBackground::Terminal {
            self.written_width = self.written_width.max(end_x);
            self.written_height = self.written_height.max(end_y);
        } else if bits != 0 {
            let highest_bit = (0..6)
                .rev()
                .find(|bit| bits & (1 << bit) != 0)
                .expect("bits is nonzero and has a highest bit");
            let set_end_y = self
                .y
                .checked_add(highest_bit + 1)
                .ok_or_else(invalid_dimensions)?;
            if set_end_y > MAX_IMAGE_SIDE {
                return Err(image_too_large());
            }
            self.set_width = self.set_width.max(end_x);
            self.set_height = self.set_height.max(set_end_y);
        }
        let (logical_width, logical_height) = self.logical_extent();
        let required_width = logical_width.max(self.declared_width.unwrap_or(0));
        let required_height = logical_height.max(self.declared_height.unwrap_or(0));
        if required_width != 0 && required_height != 0 {
            self.ensure_size(required_width, required_height)?;
        }

        if self.background == SixelBackground::Terminal {
            for offset in 0..repeat {
                let x = self.x + offset;
                for bit in 0..6 {
                    let y = self.y + bit;
                    if bits & (1 << bit) != 0 {
                        self.indices[y * self.width + x] = self.color;
                    } else {
                        let index = &mut self.indices[y * self.width + x];
                        if *index == SIXEL_UNTOUCHED {
                            *index = SIXEL_TERMINAL_BACKGROUND;
                        }
                    }
                }
            }
        } else if bits != 0 {
            for offset in 0..repeat {
                let x = self.x + offset;
                for bit in 0..6 {
                    if bits & (1 << bit) != 0 {
                        let y = self.y + bit;
                        self.indices[y * self.width + x] = self.color;
                    }
                }
            }
        }
        self.x = end_x;
        Ok(())
    }

    fn logical_extent(&self) -> (usize, usize) {
        match self.background {
            SixelBackground::Terminal => (self.written_width, self.written_height),
            SixelBackground::Preserve => (self.set_width, self.set_height),
        }
    }

    fn carriage_return(&mut self) {
        self.x = 0;
    }

    fn new_line(&mut self) -> Result<(), GraphicsError> {
        self.x = 0;
        self.y = self.y.checked_add(6).ok_or_else(invalid_dimensions)?;
        if self.y > MAX_IMAGE_SIDE {
            return Err(image_too_large());
        }
        Ok(())
    }

    fn ensure_size(
        &mut self,
        required_width: usize,
        required_height: usize,
    ) -> Result<(), GraphicsError> {
        validate_dimensions(GraphicsProtocol::Sixel, required_width, required_height)?;
        if required_width <= self.width && required_height <= self.height {
            return Ok(());
        }
        let grown_width = required_width.max(self.width.saturating_mul(2)).max(1);
        let grown_height = required_height.max(self.height.saturating_mul(2)).max(1);
        let (new_width, new_height) = if dimensions_fit(grown_width, grown_height) {
            (grown_width, grown_height)
        } else {
            (required_width, required_height)
        };
        validate_dimensions(GraphicsProtocol::Sixel, new_width, new_height)?;
        let new_len = new_width
            .checked_mul(new_height)
            .ok_or_else(invalid_dimensions)?;
        let mut indices = Vec::new();
        indices
            .try_reserve_exact(new_len)
            .map_err(|_| decode_failure())?;
        indices.resize(new_len, SIXEL_UNTOUCHED);
        let rows = self.height.min(new_height);
        let columns = self.width.min(new_width);
        for row in 0..rows {
            let old_start = row * self.width;
            let new_start = row * new_width;
            indices[new_start..new_start + columns]
                .copy_from_slice(&self.indices[old_start..old_start + columns]);
        }
        self.width = new_width;
        self.height = new_height;
        self.indices = indices;
        Ok(())
    }

    fn finish(&self) -> Result<Option<IndexedImage>, GraphicsError> {
        let data_width = if self.background == SixelBackground::Terminal {
            self.written_width
        } else {
            self.set_width
        };
        let data_height = if self.background == SixelBackground::Terminal {
            self.written_height
        } else {
            self.set_height
        };
        let width = data_width.max(self.declared_width.unwrap_or(0));
        let height = data_height.max(self.declared_height.unwrap_or(0));
        if width == 0 || height == 0 {
            return Ok(None);
        }
        validate_dimensions(GraphicsProtocol::Sixel, width, height)?;
        let length = width.checked_mul(height).ok_or_else(invalid_dimensions)?;
        let mut indices = Vec::new();
        indices
            .try_reserve_exact(length)
            .map_err(|_| decode_failure())?;
        indices.resize(length, SIXEL_UNTOUCHED);
        for row in 0..height.min(self.height) {
            let source_start = row * self.width;
            let target_start = row * width;
            let columns = width.min(self.width);
            indices[target_start..target_start + columns]
                .copy_from_slice(&self.indices[source_start..source_start + columns]);
        }

        if self.background == SixelBackground::Terminal
            && (self.declared_width.is_some() || self.declared_height.is_some())
        {
            let fill_width = self.declared_width.unwrap_or(width).min(width);
            let fill_height = self.declared_height.unwrap_or(height).min(height);
            for row in 0..fill_height {
                for column in 0..fill_width {
                    let index = &mut indices[row * width + column];
                    if *index == SIXEL_UNTOUCHED {
                        *index = SIXEL_TERMINAL_BACKGROUND;
                    }
                }
            }
        }
        Ok(Some(IndexedImage::from_parts(
            u32::try_from(width).map_err(|_| image_too_large())?,
            u32::try_from(height).map_err(|_| image_too_large())?,
            indices,
            self.aspect_vertical,
            self.aspect_horizontal,
        )?))
    }
}

fn parse_sixel_params(data: &[u8], max: usize) -> Result<Vec<u32>, GraphicsError> {
    if data.is_empty() {
        return Ok(Vec::new());
    }
    let mut result = Vec::new();
    result
        .try_reserve(max.min(5))
        .map_err(|_| decode_failure())?;
    for part in data.split(|byte| *byte == b';') {
        if result.len() == max {
            return Err(invalid_command());
        }
        result.push(if part.is_empty() { 0 } else { parse_u32(part)? });
    }
    Ok(result)
}

fn parse_sixel_header_params(data: &[u8], max: usize) -> Result<Vec<u32>, GraphicsError> {
    if data.is_empty() {
        return Ok(Vec::new());
    }
    let mut result = Vec::new();
    result.try_reserve(max).map_err(|_| decode_failure())?;
    for part in data.split(|byte| *byte == b';') {
        if result.len() == max {
            return Err(invalid_header());
        }
        result.push(if part.is_empty() { 0 } else { parse_u32(part)? });
    }
    Ok(result)
}

fn parse_repeat_count(data: &[u8]) -> Result<u32, GraphicsError> {
    if data.is_empty() {
        return Ok(1);
    }
    let params = parse_sixel_params(data, 1)?;
    if params.len() != 1 {
        return Err(invalid_command());
    }
    Ok(params[0])
}

fn parse_declared_axis(value: u32) -> Result<Option<usize>, GraphicsError> {
    if value == 0 {
        return Ok(None);
    }
    let value = usize::try_from(value).map_err(|_| image_too_large())?;
    if value > MAX_IMAGE_SIDE {
        return Err(image_too_large());
    }
    Ok(Some(value))
}

fn parse_u32(data: &[u8]) -> Result<u32, GraphicsError> {
    if data.is_empty() || !data.iter().all(u8::is_ascii_digit) {
        return Err(invalid_command());
    }
    let mut value = 0u32;
    for &byte in data {
        value = value
            .checked_mul(10)
            .and_then(|value| value.checked_add(u32::from(byte - b'0')))
            .ok_or_else(invalid_command)?;
    }
    Ok(value)
}

fn push_bounded(target: &mut Vec<u8>, byte: u8) -> Result<(), GraphicsError> {
    if target.len() == MAX_GRAPHICS_CONTROL_BYTES {
        return Err(transfer_too_large());
    }
    target.try_reserve(1).map_err(|_| decode_failure())?;
    target.push(byte);
    Ok(())
}

fn dimensions_fit(width: usize, height: usize) -> bool {
    width <= MAX_IMAGE_SIDE
        && height <= MAX_IMAGE_SIDE
        && width
            .checked_mul(height)
            .is_some_and(|pixels| pixels <= MAX_IMAGE_PIXELS)
}

fn expanded_dimensions(
    width: usize,
    height: usize,
    aspect_vertical: u32,
    aspect_horizontal: u32,
) -> Result<(usize, usize, usize, usize), GraphicsError> {
    let aspect_vertical = usize::try_from(aspect_vertical).map_err(|_| image_too_large())?;
    let aspect_horizontal = usize::try_from(aspect_horizontal).map_err(|_| image_too_large())?;
    if aspect_vertical == 0 || aspect_horizontal == 0 {
        return Err(invalid_dimensions());
    }
    let output_width = width
        .checked_mul(aspect_horizontal)
        .ok_or_else(invalid_dimensions)?;
    let output_height = height
        .checked_mul(aspect_vertical)
        .ok_or_else(invalid_dimensions)?;
    checked_rgba_len(GraphicsProtocol::Sixel, output_width, output_height)?;
    Ok((
        output_width,
        output_height,
        aspect_vertical,
        aspect_horizontal,
    ))
}

fn gcd(mut left: u32, mut right: u32) -> u32 {
    while right != 0 {
        let remainder = left % right;
        left = right;
        right = remainder;
    }
    left
}

fn hls_to_rgb(hue: u32, lightness: u32, saturation: u32) -> [u8; 3] {
    let lightness = f64::from(lightness) / 100.0;
    if saturation == 0 {
        let channel = float_to_byte(lightness);
        return [channel, channel, channel];
    }
    let saturation = f64::from(saturation) / 100.0;
    let hue = f64::from((hue + 240) % 360) / 360.0;
    let second = if lightness <= 0.5 {
        lightness * (1.0 + saturation)
    } else {
        lightness + saturation - lightness * saturation
    };
    let first = 2.0 * lightness - second;
    [
        float_to_byte(hls_component(first, second, hue + 1.0 / 3.0)),
        float_to_byte(hls_component(first, second, hue)),
        float_to_byte(hls_component(first, second, hue - 1.0 / 3.0)),
    ]
}

fn hls_component(first: f64, second: f64, mut hue: f64) -> f64 {
    if hue < 0.0 {
        hue += 1.0;
    } else if hue > 1.0 {
        hue -= 1.0;
    }
    if hue * 6.0 < 1.0 {
        first + (second - first) * hue * 6.0
    } else if hue * 2.0 < 1.0 {
        second
    } else if hue * 3.0 < 2.0 {
        first + (second - first) * (2.0 / 3.0 - hue) * 6.0
    } else {
        first
    }
}

fn float_to_byte(value: f64) -> u8 {
    (value.clamp(0.0, 1.0) * 255.0 + 0.5) as u8
}

fn percentage_to_byte(value: u32) -> u8 {
    ((value * 255 + 50) / 100) as u8
}

fn invalid_header() -> GraphicsError {
    GraphicsError::InvalidHeader {
        protocol: GraphicsProtocol::Sixel,
    }
}

fn invalid_command() -> GraphicsError {
    GraphicsError::InvalidCommand {
        protocol: GraphicsProtocol::Sixel,
    }
}

fn transfer_too_large() -> GraphicsError {
    GraphicsError::TransferTooLarge {
        protocol: GraphicsProtocol::Sixel,
    }
}

fn image_too_large() -> GraphicsError {
    GraphicsError::ImageTooLarge {
        protocol: GraphicsProtocol::Sixel,
    }
}

fn invalid_dimensions() -> GraphicsError {
    GraphicsError::InvalidDimensions {
        protocol: GraphicsProtocol::Sixel,
    }
}

fn decode_failure() -> GraphicsError {
    GraphicsError::DecodeFailure {
        protocol: GraphicsProtocol::Sixel,
    }
}
