//! Incremental decoding of DEC Sixel data.

use std::fmt;

use koshi_image::{
    compute_rgba_byte_count, validate_image_dimensions, DecodedImage, GraphicsError,
    GraphicsProtocol, SixelBackground, MAX_GRAPHICS_CONTROL_BYTE_COUNT,
    MAX_GRAPHICS_TRANSFER_BYTE_COUNT, MAX_IMAGE_PIXEL_COUNT, MAX_IMAGE_SIDE_PIXEL_COUNT,
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
    indexed_image: Option<IndexedImage>,
    palette_changes: SixelPaletteChanges,
    sixel_background: SixelBackground,
}

impl SixelGraphic {
    /// Return the indexed image, or `None` when the payload has no image extent.
    #[must_use]
    pub fn get_indexed_image(&self) -> Option<&IndexedImage> {
        self.indexed_image.as_ref()
    }

    /// Return the palette edits made by this payload.
    #[must_use]
    pub fn get_palette_changes(&self) -> &SixelPaletteChanges {
        &self.palette_changes
    }

    /// Return the payload's zero-bit background rule.
    #[must_use]
    pub fn get_sixel_background(&self) -> SixelBackground {
        self.sixel_background
    }
}

/// Up to 256 Sixel palette register edits in first-seen order.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct SixelPaletteChanges {
    palette_changes: Vec<SixelPaletteChange>,
}

impl SixelPaletteChanges {
    /// Return edits in their first-seen register order.
    #[must_use]
    pub fn list_palette_changes(&self) -> &[SixelPaletteChange] {
        &self.palette_changes
    }

    fn set_palette_change(
        &mut self,
        register_number: u8,
        rgb_color: [u8; 3],
    ) -> Result<(), GraphicsError> {
        if let Some(palette_change) = self
            .palette_changes
            .iter_mut()
            .find(|palette_change| palette_change.register_number == register_number)
        {
            palette_change.rgb_color = rgb_color;
            return Ok(());
        }
        if self.palette_changes.len() == SIXEL_REGISTER_COUNT {
            return Err(build_image_too_large_error());
        }
        self.palette_changes
            .try_reserve(1)
            .map_err(|_| build_decode_failure_error())?;
        self.palette_changes.push(SixelPaletteChange {
            register_number,
            rgb_color,
        });
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
                let mut palette_changes = Vec::new();
                while let Some(palette_change) = sequence.next_element::<SixelPaletteChange>()? {
                    if palette_changes.len() == SIXEL_REGISTER_COUNT {
                        return Err(de::Error::custom(
                            "Sixel palette changes exceed 256 registers",
                        ));
                    }
                    if palette_changes
                        .iter()
                        .any(|existing_change: &SixelPaletteChange| {
                            existing_change.register_number == palette_change.register_number
                        })
                    {
                        return Err(de::Error::custom(
                            "Sixel palette changes contain a duplicate register",
                        ));
                    }
                    palette_changes.push(palette_change);
                }
                Ok(SixelPaletteChanges { palette_changes })
            }
        }

        deserializer.deserialize_seq(ChangesVisitor)
    }
}

/// One Sixel palette register edit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SixelPaletteChange {
    register_number: u8,
    rgb_color: [u8; 3],
}

impl SixelPaletteChange {
    /// Return the edited register number.
    #[must_use]
    pub fn get_register_number(&self) -> u8 {
        self.register_number
    }

    /// Return the edited RGB color.
    #[must_use]
    pub fn get_rgb_color(&self) -> [u8; 3] {
        self.rgb_color
    }
}

/// The 256 Sixel RGB registers used when resolving an indexed image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SixelPalette {
    register_colors: [[u8; 3]; SIXEL_REGISTER_COUNT],
}

impl Default for SixelPalette {
    fn default() -> Self {
        let mut register_colors = [[0, 0, 0]; SIXEL_REGISTER_COUNT];
        let default_register_colors = [
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
        for (register_index, (red, green, blue)) in default_register_colors.into_iter().enumerate()
        {
            register_colors[register_index] = [
                convert_color_percentage_to_byte(red),
                convert_color_percentage_to_byte(green),
                convert_color_percentage_to_byte(blue),
            ];
        }
        SixelPalette { register_colors }
    }
}

impl SixelPalette {
    /// Return one actual RGB register color.
    #[must_use]
    pub fn get_register_color(&self, register_number: u8) -> [u8; 3] {
        self.register_colors[usize::from(register_number)]
    }

    /// Apply a payload's register edits to this palette.
    pub fn apply_palette_changes(&mut self, palette_changes: &SixelPaletteChanges) {
        for palette_change in &palette_changes.palette_changes {
            self.register_colors[usize::from(palette_change.register_number)] =
                palette_change.rgb_color;
        }
    }
}

impl Serialize for SixelPalette {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let mut sequence = serializer.serialize_seq(Some(SIXEL_REGISTER_COUNT))?;
        for register_color in &self.register_colors {
            sequence.serialize_element(register_color)?;
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
                let mut register_colors = [[0, 0, 0]; SIXEL_REGISTER_COUNT];
                for register_color in &mut register_colors {
                    *register_color = sequence.next_element()?.ok_or_else(|| {
                        de::Error::custom("Sixel palette has fewer than 256 colors")
                    })?;
                }
                if sequence.next_element::<de::IgnoredAny>()?.is_some() {
                    return Err(de::Error::custom("Sixel palette has more than 256 colors"));
                }
                Ok(SixelPalette { register_colors })
            }
        }

        deserializer.deserialize_seq(PaletteVisitor)
    }
}

/// A bounded row-major image of Sixel register indices before palette resolution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct IndexedImage {
    width_pixels: u32,
    height_pixels: u32,
    pixel_register_indices: Vec<u16>,
    pixel_aspect_vertical: u32,
    pixel_aspect_horizontal: u32,
}

impl IndexedImage {
    /// Return the raw indexed width.
    #[must_use]
    pub fn get_width_pixels(&self) -> u32 {
        self.width_pixels
    }

    /// Return the raw indexed height.
    #[must_use]
    pub fn get_height_pixels(&self) -> u32 {
        self.height_pixels
    }

    /// Return the normalized `(vertical, horizontal)` pixel aspect ratio.
    #[must_use]
    pub fn get_pixel_aspect_ratio(&self) -> (u32, u32) {
        (self.pixel_aspect_vertical, self.pixel_aspect_horizontal)
    }

    /// Resolve register indices and background cells into RGBA pixels.
    ///
    /// Uses `background` for terminal-background cells and expands each raw
    /// pixel by the normalized pixel aspect. Returns a graphics error for
    /// invalid or oversized dimensions, an invalid stored index, or an RGBA
    /// allocation failure.
    pub fn resolve_indexed_image(
        &self,
        palette: &SixelPalette,
        terminal_background_color: [u8; 3],
    ) -> Result<DecodedImage, GraphicsError> {
        let indexed_width_pixels =
            usize::try_from(self.width_pixels).map_err(|_| build_image_too_large_error())?;
        let indexed_height_pixels =
            usize::try_from(self.height_pixels).map_err(|_| build_image_too_large_error())?;
        let (
            resolved_width_pixels,
            resolved_height_pixels,
            resolved_pixel_aspect_vertical,
            resolved_pixel_aspect_horizontal,
        ) = expand_indexed_dimensions(
            indexed_width_pixels,
            indexed_height_pixels,
            self.pixel_aspect_vertical,
            self.pixel_aspect_horizontal,
        )?;
        let resolved_rgba_byte_count = compute_rgba_byte_count(
            GraphicsProtocol::Sixel,
            resolved_width_pixels,
            resolved_height_pixels,
        )?;
        let mut rgba_bytes = Vec::new();
        rgba_bytes
            .try_reserve_exact(resolved_rgba_byte_count)
            .map_err(|_| build_decode_failure_error())?;
        rgba_bytes.resize(resolved_rgba_byte_count, 0);

        for indexed_row in 0..indexed_height_pixels {
            for indexed_column in 0..indexed_width_pixels {
                let pixel_register_index = self.pixel_register_indices
                    [indexed_row * indexed_width_pixels + indexed_column];
                let pixel_rgba_bytes = match pixel_register_index {
                    0..=255 => {
                        let [red, green, blue] =
                            palette.register_colors[usize::from(pixel_register_index)];
                        [red, green, blue, 255]
                    }
                    SIXEL_UNTOUCHED => [0, 0, 0, 0],
                    SIXEL_TERMINAL_BACKGROUND => [
                        terminal_background_color[0],
                        terminal_background_color[1],
                        terminal_background_color[2],
                        255,
                    ],
                    _ => return Err(build_decode_failure_error()),
                };
                let first_resolved_row = indexed_row * resolved_pixel_aspect_vertical;
                let first_resolved_column = indexed_column * resolved_pixel_aspect_horizontal;
                for resolved_row in
                    first_resolved_row..first_resolved_row + resolved_pixel_aspect_vertical
                {
                    let resolved_row_byte_offset =
                        (resolved_row * resolved_width_pixels + first_resolved_column) * 4;
                    for resolved_column_offset in 0..resolved_pixel_aspect_horizontal {
                        let resolved_byte_offset =
                            resolved_row_byte_offset + resolved_column_offset * 4;
                        rgba_bytes[resolved_byte_offset..resolved_byte_offset + 4]
                            .copy_from_slice(&pixel_rgba_bytes);
                    }
                }
            }
        }

        Ok(DecodedImage {
            pixel_width: u32::try_from(resolved_width_pixels)
                .map_err(|_| build_image_too_large_error())?,
            pixel_height: u32::try_from(resolved_height_pixels)
                .map_err(|_| build_image_too_large_error())?,
            rgba_bytes,
        })
    }

    fn from_indexed_components(
        width_pixels: u32,
        height_pixels: u32,
        pixel_register_indices: Vec<u16>,
        pixel_aspect_vertical: u32,
        pixel_aspect_horizontal: u32,
    ) -> Result<Self, GraphicsError> {
        let width_pixel_count =
            usize::try_from(width_pixels).map_err(|_| build_image_too_large_error())?;
        let height_pixel_count =
            usize::try_from(height_pixels).map_err(|_| build_image_too_large_error())?;
        validate_image_dimensions(
            GraphicsProtocol::Sixel,
            width_pixel_count,
            height_pixel_count,
        )?;
        if pixel_aspect_vertical == 0
            || pixel_aspect_horizontal == 0
            || compute_greatest_common_divisor(pixel_aspect_vertical, pixel_aspect_horizontal) != 1
        {
            return Err(build_invalid_dimensions_error());
        }
        let expected_pixel_count = width_pixel_count
            .checked_mul(height_pixel_count)
            .ok_or_else(build_invalid_dimensions_error)?;
        if pixel_register_indices.len() != expected_pixel_count
            || pixel_register_indices
                .iter()
                .copied()
                .any(|register_index| register_index > SIXEL_MAX_INDEX)
        {
            return Err(build_invalid_dimensions_error());
        }
        expand_indexed_dimensions(
            width_pixel_count,
            height_pixel_count,
            pixel_aspect_vertical,
            pixel_aspect_horizontal,
        )?;
        Ok(IndexedImage {
            width_pixels,
            height_pixels,
            pixel_register_indices,
            pixel_aspect_vertical,
            pixel_aspect_horizontal,
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
            width_pixels: u32,
            height_pixels: u32,
            pixel_register_indices: BoundedIndices,
            pixel_aspect_vertical: u32,
            pixel_aspect_horizontal: u32,
        }

        let indexed_image_fields = IndexedImageFields::deserialize(deserializer)?;
        IndexedImage::from_indexed_components(
            indexed_image_fields.width_pixels,
            indexed_image_fields.height_pixels,
            indexed_image_fields.pixel_register_indices.0,
            indexed_image_fields.pixel_aspect_vertical,
            indexed_image_fields.pixel_aspect_horizontal,
        )
        .map_err(|parse_error| de::Error::custom(parse_error.to_string()))
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
                let mut pixel_register_indices = Vec::new();
                while let Some(register_index) = sequence.next_element::<u16>()? {
                    if register_index > SIXEL_MAX_INDEX {
                        return Err(de::Error::custom(
                            "Sixel indexed pixels contain an invalid register or sentinel",
                        ));
                    }
                    if pixel_register_indices.len() == MAX_IMAGE_PIXEL_COUNT {
                        return Err(de::Error::custom(
                            "Sixel indexed pixels exceed the image pixel limit",
                        ));
                    }
                    if pixel_register_indices.len() == pixel_register_indices.capacity() {
                        if pixel_register_indices.capacity() >= MAX_IMAGE_PIXEL_COUNT {
                            return Err(de::Error::custom(
                                "Sixel indexed pixels exceed the image pixel limit",
                            ));
                        }
                        let target_capacity = pixel_register_indices
                            .capacity()
                            .saturating_mul(2)
                            .clamp(1, MAX_IMAGE_PIXEL_COUNT);
                        pixel_register_indices
                            .try_reserve_exact(target_capacity - pixel_register_indices.len())
                            .map_err(|_| {
                                de::Error::custom("Sixel indexed pixels cannot be allocated")
                            })?;
                    }
                    pixel_register_indices.push(register_index);
                }
                Ok(BoundedIndices(pixel_register_indices))
            }
        }

        deserializer.deserialize_seq(IndicesVisitor)
    }
}

/// Incremental decoder for one Sixel DCS payload.
///
/// The owning terminal parser passes DCS parameter bytes, `q`, and body bytes
/// to this type and handles the string terminator. `phase` reports header or
/// body parsing, and `is_escaped` carries a split-terminator state.
#[derive(Debug, Clone)]
pub struct SixelParser {
    /// Whether the parser is collecting header parameters or body data.
    pub phase: SixelPhase,
    header_bytes: Vec<u8>,
    command: Option<SixelCommand>,
    command_parameter_bytes: Vec<u8>,
    canvas: SixelCanvas,
    /// Whether the owning DCS parser has just received `ESC`.
    pub is_escaped: bool,
    received_byte_count: usize,
}

impl SixelParser {
    /// Create an empty Sixel payload parser.
    #[must_use]
    pub fn new() -> Self {
        SixelParser {
            phase: SixelPhase::Header,
            header_bytes: Vec::new(),
            command: None,
            command_parameter_bytes: Vec::new(),
            canvas: SixelCanvas::new(),
            is_escaped: false,
            received_byte_count: 0,
        }
    }

    /// Feed one byte from a Sixel DCS, including header parameters and `q`.
    ///
    /// The string terminator is handled by the owning terminal parser. Returns
    /// a graphics error when the byte or accumulated payload is invalid or too
    /// large, or when bounded storage cannot be allocated.
    pub fn feed_input_byte(&mut self, input_byte: u8) -> Result<(), GraphicsError> {
        if self.received_byte_count == MAX_GRAPHICS_TRANSFER_BYTE_COUNT {
            return Err(build_transfer_too_large_error());
        }
        self.received_byte_count += 1;
        if input_byte == 0 || input_byte.is_ascii_whitespace() {
            return Ok(());
        }

        if self.phase == SixelPhase::Header {
            if input_byte == b'q' {
                self.parse_header()?;
                self.phase = SixelPhase::Body;
            } else if input_byte.is_ascii_digit() || input_byte == b';' {
                append_bounded_control_byte(&mut self.header_bytes, input_byte)?;
            } else {
                return Err(build_invalid_header_error());
            }
            return Ok(());
        }

        if let Some(command) = self.command {
            if input_byte.is_ascii_digit() || input_byte == b';' {
                append_bounded_control_byte(&mut self.command_parameter_bytes, input_byte)?;
                return Ok(());
            }
            self.finish_command(command)?;
        }

        if self.canvas.is_repeat_pending && !(b'?'..=b'~').contains(&input_byte) {
            return Err(build_invalid_command_error());
        }

        match input_byte {
            b'!' => {
                self.command = Some(SixelCommand::Repeat);
                self.command_parameter_bytes.clear();
            }
            b'"' => {
                if self.canvas.has_started_pixel_data {
                    return Err(build_invalid_command_error());
                }
                self.command = Some(SixelCommand::Raster);
                self.command_parameter_bytes.clear();
            }
            b'#' => {
                self.command = Some(SixelCommand::Color);
                self.command_parameter_bytes.clear();
            }
            b'$' => self.canvas.move_to_line_start(),
            b'-' => self.canvas.advance_sixel_band()?,
            b'?'..=b'~' => self.canvas.paint_sixel(input_byte - b'?')?,
            _ => return Err(build_invalid_command_error()),
        }
        Ok(())
    }

    /// Finish the payload and return its indexed image and protocol metadata.
    ///
    /// Returns `indexed_image() == None` when the payload has no image extent, or an
    /// error when the header, command, or image dimensions are incomplete or
    /// invalid, or when bounded storage cannot be allocated.
    pub fn finish_payload(mut self) -> Result<SixelGraphic, GraphicsError> {
        if let Some(command) = self.command.take() {
            self.finish_command(command)?;
        }
        if self.canvas.is_repeat_pending || self.phase != SixelPhase::Body {
            return Err(build_invalid_command_error());
        }
        Ok(SixelGraphic {
            indexed_image: self.canvas.finish_indexed_image()?,
            palette_changes: self.canvas.palette_changes,
            sixel_background: self.canvas.background,
        })
    }

    fn parse_header(&mut self) -> Result<(), GraphicsError> {
        let header_parameters = parse_sixel_header_parameters(&self.header_bytes, 3)?;
        let macro_aspect_parameter = header_parameters.first().copied().unwrap_or(0);
        if macro_aspect_parameter > 9 {
            return Err(build_invalid_command_error());
        }
        match header_parameters.get(1).copied().unwrap_or(0) {
            0 | 2 => self.canvas.background = SixelBackground::Terminal,
            1 => self.canvas.background = SixelBackground::Preserve,
            _ => return Err(build_invalid_command_error()),
        }
        self.canvas.set_macro_pixel_aspect(macro_aspect_parameter);
        Ok(())
    }

    fn finish_command(&mut self, command: SixelCommand) -> Result<(), GraphicsError> {
        let command_parameter_bytes = std::mem::take(&mut self.command_parameter_bytes);
        self.command = None;
        match command {
            SixelCommand::Repeat => {
                let repeat_count = parse_repeat_count(&command_parameter_bytes)?;
                self.canvas.set_repeat_count(repeat_count)?;
            }
            SixelCommand::Raster => {
                let raster_parameters = parse_sixel_parameters(&command_parameter_bytes, 4)?;
                self.canvas.set_raster_geometry(
                    raster_parameters.first().copied().unwrap_or(0),
                    raster_parameters.get(1).copied().unwrap_or(0),
                    raster_parameters.get(2).copied().unwrap_or(0),
                    raster_parameters.get(3).copied().unwrap_or(0),
                )?;
            }
            SixelCommand::Color => self.canvas.set_register_color(&command_parameter_bytes)?,
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
    canvas_width_pixels: usize,
    canvas_height_pixels: usize,
    pixel_register_indices: Vec<u16>,
    background: SixelBackground,
    palette_changes: SixelPaletteChanges,
    current_register_number: u16,
    cursor_column_pixels: usize,
    cursor_row_pixels: usize,
    repeat_count: usize,
    is_repeat_pending: bool,
    has_started_pixel_data: bool,
    pixel_aspect_vertical: u32,
    pixel_aspect_horizontal: u32,
    declared_width_pixels: Option<usize>,
    declared_height_pixels: Option<usize>,
    written_width_pixels: usize,
    written_height_pixels: usize,
    painted_width_pixels: usize,
    painted_height_pixels: usize,
}

impl SixelCanvas {
    fn new() -> Self {
        SixelCanvas {
            canvas_width_pixels: 0,
            canvas_height_pixels: 0,
            pixel_register_indices: Vec::new(),
            background: SixelBackground::Terminal,
            palette_changes: SixelPaletteChanges::default(),
            current_register_number: 0,
            cursor_column_pixels: 0,
            cursor_row_pixels: 0,
            repeat_count: 1,
            is_repeat_pending: false,
            has_started_pixel_data: false,
            pixel_aspect_vertical: 2,
            pixel_aspect_horizontal: 1,
            declared_width_pixels: None,
            declared_height_pixels: None,
            written_width_pixels: 0,
            written_height_pixels: 0,
            painted_width_pixels: 0,
            painted_height_pixels: 0,
        }
    }

    fn set_macro_pixel_aspect(&mut self, macro_aspect_parameter: u32) {
        let (pixel_aspect_vertical, pixel_aspect_horizontal) = match macro_aspect_parameter {
            2 => (5, 1),
            3 | 4 => (3, 1),
            5 | 6 => (2, 1),
            7..=9 => (1, 1),
            _ => (2, 1),
        };
        self.pixel_aspect_vertical = pixel_aspect_vertical;
        self.pixel_aspect_horizontal = pixel_aspect_horizontal;
    }

    fn set_repeat_count(&mut self, requested_repeat_count: u32) -> Result<(), GraphicsError> {
        let repeat_count = if requested_repeat_count == 0 {
            1
        } else {
            requested_repeat_count
        };
        let repeat_count =
            usize::try_from(repeat_count).map_err(|_| build_image_too_large_error())?;
        if repeat_count > MAX_IMAGE_SIDE_PIXEL_COUNT {
            return Err(build_image_too_large_error());
        }
        self.repeat_count = repeat_count;
        self.is_repeat_pending = true;
        Ok(())
    }

    fn set_raster_geometry(
        &mut self,
        raster_vertical_aspect: u32,
        raster_horizontal_aspect: u32,
        declared_width_pixels: u32,
        declared_height_pixels: u32,
    ) -> Result<(), GraphicsError> {
        if self.has_started_pixel_data {
            return Err(build_invalid_command_error());
        }
        let raster_vertical_aspect = if raster_vertical_aspect == 0 {
            1
        } else {
            raster_vertical_aspect
        };
        let raster_horizontal_aspect = if raster_horizontal_aspect == 0 {
            1
        } else {
            raster_horizontal_aspect
        };
        let aspect_divisor =
            compute_greatest_common_divisor(raster_vertical_aspect, raster_horizontal_aspect);
        self.pixel_aspect_vertical = raster_vertical_aspect / aspect_divisor;
        self.pixel_aspect_horizontal = raster_horizontal_aspect / aspect_divisor;
        self.declared_width_pixels = parse_declared_axis_pixels(declared_width_pixels)?;
        self.declared_height_pixels = parse_declared_axis_pixels(declared_height_pixels)?;
        Ok(())
    }

    fn set_register_color(&mut self, command_parameter_bytes: &[u8]) -> Result<(), GraphicsError> {
        let color_parameter_values = parse_sixel_parameters(command_parameter_bytes, 5)?;
        if color_parameter_values.is_empty() || color_parameter_values[0] > 255 {
            return Err(build_invalid_command_error());
        }
        let register_number =
            u8::try_from(color_parameter_values[0]).map_err(|_| build_invalid_command_error())?;
        match color_parameter_values.len() {
            1 => {
                self.current_register_number = u16::from(register_number);
            }
            5 => {
                let rgb_color = match color_parameter_values[1] {
                    1 if color_parameter_values[2] <= 360
                        && color_parameter_values[3] <= 100
                        && color_parameter_values[4] <= 100 =>
                    {
                        convert_hls_to_rgb(
                            color_parameter_values[2],
                            color_parameter_values[3],
                            color_parameter_values[4],
                        )
                    }
                    2 if color_parameter_values[2] <= 100
                        && color_parameter_values[3] <= 100
                        && color_parameter_values[4] <= 100 =>
                    {
                        [
                            convert_color_percentage_to_byte(color_parameter_values[2]),
                            convert_color_percentage_to_byte(color_parameter_values[3]),
                            convert_color_percentage_to_byte(color_parameter_values[4]),
                        ]
                    }
                    _ => return Err(build_invalid_command_error()),
                };
                self.palette_changes
                    .set_palette_change(register_number, rgb_color)?;
                self.current_register_number = u16::from(register_number);
            }
            _ => return Err(build_invalid_command_error()),
        }
        Ok(())
    }

    fn paint_sixel(&mut self, sixel_bit_mask: u8) -> Result<(), GraphicsError> {
        let repeat_count = self.repeat_count;
        self.repeat_count = 1;
        self.is_repeat_pending = false;
        let end_column_pixels = self
            .cursor_column_pixels
            .checked_add(repeat_count)
            .ok_or_else(build_invalid_dimensions_error)?;
        if end_column_pixels > MAX_IMAGE_SIDE_PIXEL_COUNT {
            return Err(build_image_too_large_error());
        }
        let end_row_pixels = self
            .cursor_row_pixels
            .checked_add(6)
            .ok_or_else(build_invalid_dimensions_error)?;
        if self.background == SixelBackground::Terminal
            && end_row_pixels > MAX_IMAGE_SIDE_PIXEL_COUNT
        {
            return Err(build_image_too_large_error());
        }
        self.has_started_pixel_data = true;

        if self.background == SixelBackground::Terminal {
            self.written_width_pixels = self.written_width_pixels.max(end_column_pixels);
            self.written_height_pixels = self.written_height_pixels.max(end_row_pixels);
        } else if sixel_bit_mask != 0 {
            let highest_bit_offset = (0..6)
                .rev()
                .find(|bit_offset| sixel_bit_mask & (1 << bit_offset) != 0)
                .expect("a nonzero Sixel bit mask has a highest bit");
            let set_end_row = self
                .cursor_row_pixels
                .checked_add(highest_bit_offset + 1)
                .ok_or_else(build_invalid_dimensions_error)?;
            if set_end_row > MAX_IMAGE_SIDE_PIXEL_COUNT {
                return Err(build_image_too_large_error());
            }
            self.painted_width_pixels = self.painted_width_pixels.max(end_column_pixels);
            self.painted_height_pixels = self.painted_height_pixels.max(set_end_row);
        }
        let (logical_width_pixels, logical_height_pixels) = self.get_logical_extent_pixels();
        let required_width_pixels =
            logical_width_pixels.max(self.declared_width_pixels.unwrap_or(0));
        let required_height_pixels =
            logical_height_pixels.max(self.declared_height_pixels.unwrap_or(0));
        if required_width_pixels != 0 && required_height_pixels != 0 {
            self.ensure_canvas_size(required_width_pixels, required_height_pixels)?;
        }

        if self.background == SixelBackground::Terminal {
            for column_offset in 0..repeat_count {
                let image_column_pixels = self.cursor_column_pixels + column_offset;
                for row_offset in 0..6 {
                    let image_row_pixels = self.cursor_row_pixels + row_offset;
                    if sixel_bit_mask & (1 << row_offset) != 0 {
                        self.pixel_register_indices
                            [image_row_pixels * self.canvas_width_pixels + image_column_pixels] =
                            self.current_register_number;
                    } else {
                        let register_index = &mut self.pixel_register_indices
                            [image_row_pixels * self.canvas_width_pixels + image_column_pixels];
                        if *register_index == SIXEL_UNTOUCHED {
                            *register_index = SIXEL_TERMINAL_BACKGROUND;
                        }
                    }
                }
            }
        } else if sixel_bit_mask != 0 {
            for column_offset in 0..repeat_count {
                let image_column_pixels = self.cursor_column_pixels + column_offset;
                for row_offset in 0..6 {
                    if sixel_bit_mask & (1 << row_offset) != 0 {
                        let image_row_pixels = self.cursor_row_pixels + row_offset;
                        self.pixel_register_indices
                            [image_row_pixels * self.canvas_width_pixels + image_column_pixels] =
                            self.current_register_number;
                    }
                }
            }
        }
        self.cursor_column_pixels = end_column_pixels;
        Ok(())
    }

    fn get_logical_extent_pixels(&self) -> (usize, usize) {
        match self.background {
            SixelBackground::Terminal => (self.written_width_pixels, self.written_height_pixels),
            SixelBackground::Preserve => (self.painted_width_pixels, self.painted_height_pixels),
        }
    }

    fn move_to_line_start(&mut self) {
        self.cursor_column_pixels = 0;
    }

    fn advance_sixel_band(&mut self) -> Result<(), GraphicsError> {
        self.cursor_column_pixels = 0;
        self.cursor_row_pixels = self
            .cursor_row_pixels
            .checked_add(6)
            .ok_or_else(build_invalid_dimensions_error)?;
        if self.cursor_row_pixels > MAX_IMAGE_SIDE_PIXEL_COUNT {
            return Err(build_image_too_large_error());
        }
        Ok(())
    }

    fn ensure_canvas_size(
        &mut self,
        required_width_pixels: usize,
        required_height_pixels: usize,
    ) -> Result<(), GraphicsError> {
        validate_image_dimensions(
            GraphicsProtocol::Sixel,
            required_width_pixels,
            required_height_pixels,
        )?;
        if required_width_pixels <= self.canvas_width_pixels
            && required_height_pixels <= self.canvas_height_pixels
        {
            return Ok(());
        }
        let grown_width_pixels = required_width_pixels
            .max(self.canvas_width_pixels.saturating_mul(2))
            .max(1);
        let grown_height_pixels = required_height_pixels
            .max(self.canvas_height_pixels.saturating_mul(2))
            .max(1);
        let (new_width_pixels, new_height_pixels) =
            if can_image_dimensions_fit_limits(grown_width_pixels, grown_height_pixels) {
                (grown_width_pixels, grown_height_pixels)
            } else {
                (required_width_pixels, required_height_pixels)
            };
        validate_image_dimensions(GraphicsProtocol::Sixel, new_width_pixels, new_height_pixels)?;
        let new_pixel_count = new_width_pixels
            .checked_mul(new_height_pixels)
            .ok_or_else(build_invalid_dimensions_error)?;
        let mut pixel_register_indices = Vec::new();
        pixel_register_indices
            .try_reserve_exact(new_pixel_count)
            .map_err(|_| build_decode_failure_error())?;
        pixel_register_indices.resize(new_pixel_count, SIXEL_UNTOUCHED);
        let copied_row_count = self.canvas_height_pixels.min(new_height_pixels);
        let copied_column_count = self.canvas_width_pixels.min(new_width_pixels);
        for row_index in 0..copied_row_count {
            let old_row_start = row_index * self.canvas_width_pixels;
            let new_row_start = row_index * new_width_pixels;
            pixel_register_indices[new_row_start..new_row_start + copied_column_count]
                .copy_from_slice(
                    &self.pixel_register_indices
                        [old_row_start..old_row_start + copied_column_count],
                );
        }
        self.canvas_width_pixels = new_width_pixels;
        self.canvas_height_pixels = new_height_pixels;
        self.pixel_register_indices = pixel_register_indices;
        Ok(())
    }

    fn finish_indexed_image(&self) -> Result<Option<IndexedImage>, GraphicsError> {
        let (logical_width_pixels, logical_height_pixels) = self.get_logical_extent_pixels();
        let width_pixels = logical_width_pixels.max(self.declared_width_pixels.unwrap_or(0));
        let height_pixels = logical_height_pixels.max(self.declared_height_pixels.unwrap_or(0));
        if width_pixels == 0 || height_pixels == 0 {
            return Ok(None);
        }
        validate_image_dimensions(GraphicsProtocol::Sixel, width_pixels, height_pixels)?;
        let pixel_count = width_pixels
            .checked_mul(height_pixels)
            .ok_or_else(build_invalid_dimensions_error)?;
        let mut pixel_register_indices = Vec::new();
        pixel_register_indices
            .try_reserve_exact(pixel_count)
            .map_err(|_| build_decode_failure_error())?;
        pixel_register_indices.resize(pixel_count, SIXEL_UNTOUCHED);
        for row_index in 0..height_pixels.min(self.canvas_height_pixels) {
            let source_row_start = row_index * self.canvas_width_pixels;
            let target_row_start = row_index * width_pixels;
            let copied_column_count = width_pixels.min(self.canvas_width_pixels);
            pixel_register_indices[target_row_start..target_row_start + copied_column_count]
                .copy_from_slice(
                    &self.pixel_register_indices
                        [source_row_start..source_row_start + copied_column_count],
                );
        }

        if self.background == SixelBackground::Terminal
            && (self.declared_width_pixels.is_some() || self.declared_height_pixels.is_some())
        {
            let fill_width_pixels = self
                .declared_width_pixels
                .unwrap_or(width_pixels)
                .min(width_pixels);
            let fill_height_pixels = self
                .declared_height_pixels
                .unwrap_or(height_pixels)
                .min(height_pixels);
            for row_index in 0..fill_height_pixels {
                for column_index in 0..fill_width_pixels {
                    let register_index =
                        &mut pixel_register_indices[row_index * width_pixels + column_index];
                    if *register_index == SIXEL_UNTOUCHED {
                        *register_index = SIXEL_TERMINAL_BACKGROUND;
                    }
                }
            }
        }
        Ok(Some(IndexedImage::from_indexed_components(
            u32::try_from(width_pixels).map_err(|_| build_image_too_large_error())?,
            u32::try_from(height_pixels).map_err(|_| build_image_too_large_error())?,
            pixel_register_indices,
            self.pixel_aspect_vertical,
            self.pixel_aspect_horizontal,
        )?))
    }
}

fn parse_sixel_parameters(
    sixel_parameter_bytes: &[u8],
    maximum_sixel_parameter_count: usize,
) -> Result<Vec<u32>, GraphicsError> {
    if sixel_parameter_bytes.is_empty() {
        return Ok(Vec::new());
    }
    let mut sixel_parameter_values = Vec::new();
    sixel_parameter_values
        .try_reserve(maximum_sixel_parameter_count.min(5))
        .map_err(|_| build_decode_failure_error())?;
    for parameter_slice in sixel_parameter_bytes.split(|parameter_byte| *parameter_byte == b';') {
        if sixel_parameter_values.len() == maximum_sixel_parameter_count {
            return Err(build_invalid_command_error());
        }
        sixel_parameter_values.push(if parameter_slice.is_empty() {
            0
        } else {
            parse_decimal_number(parameter_slice)?
        });
    }
    Ok(sixel_parameter_values)
}

fn parse_sixel_header_parameters(
    sixel_parameter_bytes: &[u8],
    maximum_sixel_parameter_count: usize,
) -> Result<Vec<u32>, GraphicsError> {
    if sixel_parameter_bytes.is_empty() {
        return Ok(Vec::new());
    }
    let mut sixel_parameter_values = Vec::new();
    sixel_parameter_values
        .try_reserve(maximum_sixel_parameter_count)
        .map_err(|_| build_decode_failure_error())?;
    for parameter_slice in sixel_parameter_bytes.split(|parameter_byte| *parameter_byte == b';') {
        if sixel_parameter_values.len() == maximum_sixel_parameter_count {
            return Err(build_invalid_header_error());
        }
        sixel_parameter_values.push(if parameter_slice.is_empty() {
            0
        } else {
            parse_decimal_number(parameter_slice)?
        });
    }
    Ok(sixel_parameter_values)
}

fn parse_repeat_count(command_parameter_bytes: &[u8]) -> Result<u32, GraphicsError> {
    if command_parameter_bytes.is_empty() {
        return Ok(1);
    }
    let repeat_parameters = parse_sixel_parameters(command_parameter_bytes, 1)?;
    if repeat_parameters.len() != 1 {
        return Err(build_invalid_command_error());
    }
    Ok(repeat_parameters[0])
}

fn parse_declared_axis_pixels(declared_axis_number: u32) -> Result<Option<usize>, GraphicsError> {
    if declared_axis_number == 0 {
        return Ok(None);
    }
    let declared_axis_pixels =
        usize::try_from(declared_axis_number).map_err(|_| build_image_too_large_error())?;
    if declared_axis_pixels > MAX_IMAGE_SIDE_PIXEL_COUNT {
        return Err(build_image_too_large_error());
    }
    Ok(Some(declared_axis_pixels))
}

fn parse_decimal_number(decimal_bytes: &[u8]) -> Result<u32, GraphicsError> {
    if decimal_bytes.is_empty() || !decimal_bytes.iter().all(u8::is_ascii_digit) {
        return Err(build_invalid_command_error());
    }
    let mut decimal_number = 0u32;
    for &decimal_byte in decimal_bytes {
        decimal_number = decimal_number
            .checked_mul(10)
            .and_then(|current_decimal_number| {
                current_decimal_number.checked_add(u32::from(decimal_byte - b'0'))
            })
            .ok_or_else(build_invalid_command_error)?;
    }
    Ok(decimal_number)
}

fn append_bounded_control_byte(
    control_bytes: &mut Vec<u8>,
    control_byte: u8,
) -> Result<(), GraphicsError> {
    if control_bytes.len() == MAX_GRAPHICS_CONTROL_BYTE_COUNT {
        return Err(build_transfer_too_large_error());
    }
    control_bytes
        .try_reserve(1)
        .map_err(|_| build_decode_failure_error())?;
    control_bytes.push(control_byte);
    Ok(())
}

fn can_image_dimensions_fit_limits(image_width_pixels: usize, image_height_pixels: usize) -> bool {
    image_width_pixels <= MAX_IMAGE_SIDE_PIXEL_COUNT
        && image_height_pixels <= MAX_IMAGE_SIDE_PIXEL_COUNT
        && image_width_pixels
            .checked_mul(image_height_pixels)
            .is_some_and(|pixel_count| pixel_count <= MAX_IMAGE_PIXEL_COUNT)
}

fn expand_indexed_dimensions(
    indexed_width_pixels: usize,
    indexed_height_pixels: usize,
    aspect_vertical: u32,
    aspect_horizontal: u32,
) -> Result<(usize, usize, usize, usize), GraphicsError> {
    let aspect_vertical =
        usize::try_from(aspect_vertical).map_err(|_| build_image_too_large_error())?;
    let aspect_horizontal =
        usize::try_from(aspect_horizontal).map_err(|_| build_image_too_large_error())?;
    if aspect_vertical == 0 || aspect_horizontal == 0 {
        return Err(build_invalid_dimensions_error());
    }
    let resolved_width_pixels = indexed_width_pixels
        .checked_mul(aspect_horizontal)
        .ok_or_else(build_invalid_dimensions_error)?;
    let resolved_height_pixels = indexed_height_pixels
        .checked_mul(aspect_vertical)
        .ok_or_else(build_invalid_dimensions_error)?;
    compute_rgba_byte_count(
        GraphicsProtocol::Sixel,
        resolved_width_pixels,
        resolved_height_pixels,
    )?;
    Ok((
        resolved_width_pixels,
        resolved_height_pixels,
        aspect_vertical,
        aspect_horizontal,
    ))
}

fn compute_greatest_common_divisor(mut left: u32, mut right: u32) -> u32 {
    while right != 0 {
        let remainder = left % right;
        left = right;
        right = remainder;
    }
    left
}

fn convert_hls_to_rgb(
    hue_degrees: u32,
    lightness_percent: u32,
    saturation_percent: u32,
) -> [u8; 3] {
    let lightness = f64::from(lightness_percent) / 100.0;
    if saturation_percent == 0 {
        let channel = convert_color_component_fraction_to_byte(lightness);
        return [channel, channel, channel];
    }
    let saturation = f64::from(saturation_percent) / 100.0;
    let hue = f64::from((hue_degrees + 240) % 360) / 360.0;
    let upper_color_component = if lightness <= 0.5 {
        lightness * (1.0 + saturation)
    } else {
        lightness + saturation - lightness * saturation
    };
    let lower_color_component = 2.0 * lightness - upper_color_component;
    [
        convert_color_component_fraction_to_byte(compute_hls_component(
            lower_color_component,
            upper_color_component,
            hue + 1.0 / 3.0,
        )),
        convert_color_component_fraction_to_byte(compute_hls_component(
            lower_color_component,
            upper_color_component,
            hue,
        )),
        convert_color_component_fraction_to_byte(compute_hls_component(
            lower_color_component,
            upper_color_component,
            hue - 1.0 / 3.0,
        )),
    ]
}

fn compute_hls_component(
    lower_color_component: f64,
    upper_color_component: f64,
    mut hue_position: f64,
) -> f64 {
    if hue_position < 0.0 {
        hue_position += 1.0;
    } else if hue_position > 1.0 {
        hue_position -= 1.0;
    }
    if hue_position * 6.0 < 1.0 {
        lower_color_component + (upper_color_component - lower_color_component) * hue_position * 6.0
    } else if hue_position * 2.0 < 1.0 {
        upper_color_component
    } else if hue_position * 3.0 < 2.0 {
        lower_color_component
            + (upper_color_component - lower_color_component) * (2.0 / 3.0 - hue_position) * 6.0
    } else {
        lower_color_component
    }
}

fn convert_color_component_fraction_to_byte(color_component_fraction: f64) -> u8 {
    (color_component_fraction.clamp(0.0, 1.0) * 255.0 + 0.5) as u8
}

fn convert_color_percentage_to_byte(color_percentage: u32) -> u8 {
    ((color_percentage * 255 + 50) / 100) as u8
}

fn build_invalid_header_error() -> GraphicsError {
    GraphicsError::InvalidHeader {
        protocol: GraphicsProtocol::Sixel,
    }
}

fn build_invalid_command_error() -> GraphicsError {
    GraphicsError::InvalidCommand {
        protocol: GraphicsProtocol::Sixel,
    }
}

fn build_transfer_too_large_error() -> GraphicsError {
    GraphicsError::TransferTooLarge {
        protocol: GraphicsProtocol::Sixel,
    }
}

fn build_image_too_large_error() -> GraphicsError {
    GraphicsError::ImageTooLarge {
        protocol: GraphicsProtocol::Sixel,
    }
}

fn build_invalid_dimensions_error() -> GraphicsError {
    GraphicsError::InvalidDimensions {
        protocol: GraphicsProtocol::Sixel,
    }
}

fn build_decode_failure_error() -> GraphicsError {
    GraphicsError::DecodeFailure {
        protocol: GraphicsProtocol::Sixel,
    }
}
