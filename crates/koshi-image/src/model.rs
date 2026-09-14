//! Shared image records and validated decoded pixels.

use std::sync::Arc;

use serde::de::{self, DeserializeSeed};
use serde::{Deserialize, Deserializer, Serialize};

use crate::animation::DecodedAnimation;
use crate::error::ImagePlacementError;
use crate::serde_support::BoundedBytesSeed;
use crate::{MAX_IMAGE_BYTE_COUNT, MAX_IMAGE_PIXEL_COUNT, MAX_IMAGE_SIDE_PIXEL_COUNT};

/// The terminal image protocol that produced a record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum GraphicsProtocol {
    /// DEC Sixel raster data in a DCS string.
    Sixel,
    /// Kitty graphics data in an APC string.
    Kitty,
    /// iTerm2 OSC 1337 inline image data.
    Iterm2,
}

/// A requested image dimension from a protocol display field.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ImageDimension {
    /// A number of terminal cells.
    Cells(u32),
    /// A number of device pixels.
    Pixels(u32),
    /// A percentage of the available terminal area.
    Percent(u16),
    /// Let the terminal choose the dimension.
    Auto,
}

/// The Sixel zero-bit background rule carried with an image record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SixelBackground {
    /// Use the terminal background for zero bits.
    Terminal,
    /// Keep zero bits transparent.
    Preserve,
}

/// Display hints carried by an image protocol.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageDisplay {
    /// The requested width, if the sender supplied one.
    #[serde(rename = "width")]
    pub requested_width: Option<ImageDimension>,
    /// The requested height, if the sender supplied one.
    #[serde(rename = "height")]
    pub requested_height: Option<ImageDimension>,
    /// Whether the sender requests aspect-ratio preservation.
    #[serde(rename = "preserve_aspect_ratio")]
    pub is_aspect_ratio_preserved: bool,
    /// The Sixel background rule, when the record came from Sixel.
    pub sixel_background: Option<SixelBackground>,
    /// The kitty image id, when one was supplied.
    pub image_id: Option<u32>,
    /// The kitty image number, when one was supplied.
    pub image_number: Option<u32>,
    /// The kitty placement id, when one was supplied.
    pub placement_id: Option<u32>,
    /// Usage flags supplied by kitty.
    pub usage_hints: u32,
    /// Whether kitty asks for a Unicode-placeholder placement.
    #[serde(rename = "unicode_placeholder")]
    pub is_unicode_placeholder: bool,
    /// The kitty image z-index.
    pub z_index: i32,
    /// The parent Kitty image id for a relative placement.
    #[serde(default)]
    pub relative_image_id: Option<u32>,
    /// The parent Kitty placement id for a relative placement.
    #[serde(default)]
    pub relative_placement_id: Option<u32>,
    /// The horizontal cell offset from a relative parent placement.
    #[serde(default)]
    #[serde(rename = "relative_offset_x")]
    pub relative_column_offset: i32,
    /// The vertical cell offset from a relative parent placement.
    #[serde(default)]
    #[serde(rename = "relative_offset_y")]
    pub relative_row_offset: i32,
    /// The number of terminal columns requested by kitty.
    #[serde(rename = "cell_columns")]
    pub requested_column_count: Option<u32>,
    /// The number of terminal rows requested by kitty.
    #[serde(rename = "cell_rows")]
    pub requested_row_count: Option<u32>,
    /// The source image x offset requested by kitty, in pixels.
    #[serde(rename = "source_offset_x")]
    pub source_pixel_offset_x: Option<u32>,
    /// The source image y offset requested by kitty, in pixels.
    #[serde(rename = "source_offset_y")]
    pub source_pixel_offset_y: Option<u32>,
    /// The x offset inside the first terminal cell requested by kitty.
    #[serde(rename = "cell_offset_x")]
    pub cell_pixel_offset_x: Option<u32>,
    /// The y offset inside the first terminal cell requested by kitty.
    #[serde(rename = "cell_offset_y")]
    pub cell_pixel_offset_y: Option<u32>,
    /// Whether kitty asks the placement to move the cursor after display.
    #[serde(rename = "move_cursor")]
    pub should_move_cursor: bool,
    /// Kitty response suppression: 0 sends all replies, 1 sends errors, 2 sends none.
    #[serde(default)]
    #[serde(rename = "quiet")]
    pub response_suppression_level: u8,
}

impl Default for ImageDisplay {
    fn default() -> Self {
        ImageDisplay {
            requested_width: None,
            requested_height: None,
            is_aspect_ratio_preserved: true,
            sixel_background: None,
            image_id: None,
            image_number: None,
            placement_id: None,
            usage_hints: 0,
            is_unicode_placeholder: false,
            z_index: 0,
            relative_image_id: None,
            relative_placement_id: None,
            relative_column_offset: 0,
            relative_row_offset: 0,
            requested_column_count: None,
            requested_row_count: None,
            source_pixel_offset_x: None,
            source_pixel_offset_y: None,
            cell_pixel_offset_x: None,
            cell_pixel_offset_y: None,
            should_move_cursor: true,
            response_suppression_level: 0,
        }
    }
}

/// An image stored as row-major RGBA bytes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DecodedImage {
    /// Image width in pixels.
    #[serde(rename = "width")]
    pub pixel_width: u32,
    /// Image height in pixels.
    #[serde(rename = "height")]
    pub pixel_height: u32,
    /// Four bytes per pixel in red, green, blue, alpha order.
    #[serde(rename = "rgba")]
    pub rgba_bytes: Vec<u8>,
}

impl<'de> Deserialize<'de> for DecodedImage {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct DecodedImageFields {
            #[serde(rename = "width")]
            pixel_width: u32,
            #[serde(rename = "height")]
            pixel_height: u32,
            #[serde(deserialize_with = "deserialize_rgba_bytes")]
            #[serde(rename = "rgba")]
            rgba_bytes: Vec<u8>,
        }

        let decoded_image_fields = DecodedImageFields::deserialize(deserializer)?;
        validate_decoded_image::<D::Error>(
            decoded_image_fields.pixel_width,
            decoded_image_fields.pixel_height,
            decoded_image_fields.rgba_bytes,
        )
    }
}

fn deserialize_rgba_bytes<'de, D>(deserializer: D) -> Result<Vec<u8>, D::Error>
where
    D: Deserializer<'de>,
{
    BoundedBytesSeed::from_byte_limit_and_error_label(
        MAX_IMAGE_BYTE_COUNT,
        "decoded image RGBA data",
    )
    .deserialize(deserializer)
}

fn validate_decoded_image<E>(
    decoded_pixel_width: u32,
    decoded_pixel_height: u32,
    rgba_bytes: Vec<u8>,
) -> Result<DecodedImage, E>
where
    E: de::Error,
{
    let pixel_width = usize::try_from(decoded_pixel_width)
        .map_err(|_| E::custom("decoded image width cannot be represented by this platform"))?;
    let pixel_height = usize::try_from(decoded_pixel_height)
        .map_err(|_| E::custom("decoded image height cannot be represented by this platform"))?;
    let pixel_count = pixel_width
        .checked_mul(pixel_height)
        .ok_or_else(|| E::custom("decoded image dimensions overflow"))?;
    let expected_rgba_byte_count = pixel_count
        .checked_mul(4)
        .ok_or_else(|| E::custom("decoded image byte count overflows"))?;
    if pixel_width == 0
        || pixel_height == 0
        || pixel_width > MAX_IMAGE_SIDE_PIXEL_COUNT
        || pixel_height > MAX_IMAGE_SIDE_PIXEL_COUNT
        || pixel_count > MAX_IMAGE_PIXEL_COUNT
        || expected_rgba_byte_count > MAX_IMAGE_BYTE_COUNT
    {
        return Err(E::custom("decoded image dimensions exceed graphics limits"));
    }
    if rgba_bytes.len() != expected_rgba_byte_count {
        return Err(E::custom(
            "decoded image RGBA length does not match its dimensions",
        ));
    }
    Ok(DecodedImage {
        pixel_width: decoded_pixel_width,
        pixel_height: decoded_pixel_height,
        rgba_bytes,
    })
}

/// The transfer action recorded with an image record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ImageAction {
    /// Transmit the decoded image without requesting display.
    Transmit,
    /// Place a decoded image without a Kitty image transfer.
    Display,
    /// Transmit and place the decoded image in one operation.
    TransmitAndDisplay,
}

/// A complete image transfer queued for the terminal caller.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageRecord {
    /// Protocol that supplied the image.
    pub protocol: GraphicsProtocol,
    /// Validated pixel data.
    pub image: Arc<DecodedImage>,
    /// Retained animation frames, when the transfer contains animated media.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub animation: Option<Arc<DecodedAnimation>>,
    /// The state operation represented by the transfer.
    pub action: ImageAction,
    /// Display hints supplied by the protocol.
    pub display: ImageDisplay,
    /// Cursor position when the image sequence ended, as row and column.
    pub anchor: (u16, u16),
}

impl ImageRecord {
    /// Return `(x, y, width, height)` in decoded-image pixels.
    ///
    /// Non-Kitty records use the complete image. Kitty offsets default to zero;
    /// pixel dimensions are clamped to the image, and other dimension units use
    /// the remaining pixels. A zero extent or an origin outside the image
    /// returns [`ImagePlacementError::SourceOutOfBounds`]. For an 8x6 image with
    /// Kitty `x = 2`, `y = 0`, `width = 20`, and no height, it returns `(2, 0, 6, 6)`.
    pub fn compute_source_rect(&self) -> Result<(u32, u32, u32, u32), ImagePlacementError> {
        if self.protocol != GraphicsProtocol::Kitty {
            return Ok((0, 0, self.image.pixel_width, self.image.pixel_height));
        }

        let source_pixel_offset_x = self.display.source_pixel_offset_x.unwrap_or(0);
        let source_pixel_offset_y = self.display.source_pixel_offset_y.unwrap_or(0);
        let source_pixel_width = match self.display.requested_width {
            Some(ImageDimension::Pixels(requested_width_pixels)) => requested_width_pixels,
            _ => self.image.pixel_width.saturating_sub(source_pixel_offset_x),
        };
        let source_pixel_height = match self.display.requested_height {
            Some(ImageDimension::Pixels(requested_height_pixels)) => requested_height_pixels,
            _ => self
                .image
                .pixel_height
                .saturating_sub(source_pixel_offset_y),
        };
        let is_source_rect_valid = source_pixel_width > 0
            && source_pixel_height > 0
            && source_pixel_offset_x < self.image.pixel_width
            && source_pixel_offset_y < self.image.pixel_height;
        if !is_source_rect_valid {
            return Err(ImagePlacementError::SourceOutOfBounds {
                source_x: source_pixel_offset_x,
                source_y: source_pixel_offset_y,
                source_pixel_width,
                source_pixel_height,
                image_pixel_width: self.image.pixel_width,
                image_pixel_height: self.image.pixel_height,
            });
        }
        Ok((
            source_pixel_offset_x,
            source_pixel_offset_y,
            source_pixel_width.min(self.image.pixel_width - source_pixel_offset_x),
            source_pixel_height.min(self.image.pixel_height - source_pixel_offset_y),
        ))
    }
}

/// The protocol-independent decoded result produced by a raw parser.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedGraphics {
    /// Whether the protocol request asks for a response without image display.
    pub is_query: bool,
    /// Protocol that supplied the image.
    pub protocol: GraphicsProtocol,
    /// Validated image pixels.
    pub image: DecodedImage,
    /// Retained animation frames, when the transfer contains animated media.
    pub animation: Option<DecodedAnimation>,
    /// The state operation represented by the transfer.
    pub action: ImageAction,
    /// Display hints from the transfer.
    pub display: ImageDisplay,
}

#[cfg(test)]
mod tests;
