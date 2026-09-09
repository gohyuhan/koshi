//! Shared image records and validated decoded pixels.

use std::sync::Arc;

use serde::de::{self, DeserializeSeed};
use serde::{Deserialize, Deserializer, Serialize};

use crate::animation::DecodedAnimation;
use crate::error::ImagePlacementError;
use crate::serde_support::BoundedBytesSeed;
use crate::{MAX_IMAGE_BYTES, MAX_IMAGE_PIXELS, MAX_IMAGE_SIDE};

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
    pub width: Option<ImageDimension>,
    /// The requested height, if the sender supplied one.
    pub height: Option<ImageDimension>,
    /// Whether the sender requests aspect-ratio preservation.
    pub preserve_aspect_ratio: bool,
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
    pub unicode_placeholder: bool,
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
    pub relative_offset_x: i32,
    /// The vertical cell offset from a relative parent placement.
    #[serde(default)]
    pub relative_offset_y: i32,
    /// The number of terminal columns requested by kitty.
    pub cell_columns: Option<u32>,
    /// The number of terminal rows requested by kitty.
    pub cell_rows: Option<u32>,
    /// The source image x offset requested by kitty, in pixels.
    pub source_offset_x: Option<u32>,
    /// The source image y offset requested by kitty, in pixels.
    pub source_offset_y: Option<u32>,
    /// The x offset inside the first terminal cell requested by kitty.
    pub cell_offset_x: Option<u32>,
    /// The y offset inside the first terminal cell requested by kitty.
    pub cell_offset_y: Option<u32>,
    /// Whether kitty asks the placement to move the cursor after display.
    pub move_cursor: bool,
    /// Kitty response suppression: 0 sends all replies, 1 sends errors, 2 sends none.
    #[serde(default)]
    pub quiet: u8,
}

impl Default for ImageDisplay {
    fn default() -> Self {
        ImageDisplay {
            width: None,
            height: None,
            preserve_aspect_ratio: true,
            sixel_background: None,
            image_id: None,
            image_number: None,
            placement_id: None,
            usage_hints: 0,
            unicode_placeholder: false,
            z_index: 0,
            relative_image_id: None,
            relative_placement_id: None,
            relative_offset_x: 0,
            relative_offset_y: 0,
            cell_columns: None,
            cell_rows: None,
            source_offset_x: None,
            source_offset_y: None,
            cell_offset_x: None,
            cell_offset_y: None,
            move_cursor: true,
            quiet: 0,
        }
    }
}

/// An image stored as row-major RGBA bytes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DecodedImage {
    /// Image width in pixels.
    pub width: u32,
    /// Image height in pixels.
    pub height: u32,
    /// Four bytes per pixel in red, green, blue, alpha order.
    pub rgba: Vec<u8>,
}

impl<'de> Deserialize<'de> for DecodedImage {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct DecodedImageFields {
            width: u32,
            height: u32,
            #[serde(deserialize_with = "deserialize_rgba")]
            rgba: Vec<u8>,
        }

        let fields = DecodedImageFields::deserialize(deserializer)?;
        validate_decoded_image::<D::Error>(fields.width, fields.height, fields.rgba)
    }
}

fn deserialize_rgba<'de, D>(deserializer: D) -> Result<Vec<u8>, D::Error>
where
    D: Deserializer<'de>,
{
    BoundedBytesSeed::new(MAX_IMAGE_BYTES, "decoded image RGBA data").deserialize(deserializer)
}

fn validate_decoded_image<E>(width: u32, height: u32, rgba: Vec<u8>) -> Result<DecodedImage, E>
where
    E: de::Error,
{
    let width_usize = usize::try_from(width)
        .map_err(|_| E::custom("decoded image width cannot be represented by this platform"))?;
    let height_usize = usize::try_from(height)
        .map_err(|_| E::custom("decoded image height cannot be represented by this platform"))?;
    let pixels = width_usize
        .checked_mul(height_usize)
        .ok_or_else(|| E::custom("decoded image dimensions overflow"))?;
    let expected_bytes = pixels
        .checked_mul(4)
        .ok_or_else(|| E::custom("decoded image byte count overflows"))?;
    if width_usize == 0
        || height_usize == 0
        || width_usize > MAX_IMAGE_SIDE
        || height_usize > MAX_IMAGE_SIDE
        || pixels > MAX_IMAGE_PIXELS
        || expected_bytes > MAX_IMAGE_BYTES
    {
        return Err(E::custom("decoded image dimensions exceed graphics limits"));
    }
    if rgba.len() != expected_bytes {
        return Err(E::custom(
            "decoded image RGBA length does not match its dimensions",
        ));
    }
    Ok(DecodedImage {
        width,
        height,
        rgba,
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
    pub fn source_rect(&self) -> Result<(u32, u32, u32, u32), ImagePlacementError> {
        if self.protocol != GraphicsProtocol::Kitty {
            return Ok((0, 0, self.image.width, self.image.height));
        }

        let x = self.display.source_offset_x.unwrap_or(0);
        let y = self.display.source_offset_y.unwrap_or(0);
        let width = match self.display.width {
            Some(ImageDimension::Pixels(value)) => value,
            _ => self.image.width.saturating_sub(x),
        };
        let height = match self.display.height {
            Some(ImageDimension::Pixels(value)) => value,
            _ => self.image.height.saturating_sub(y),
        };
        let valid = width > 0 && height > 0 && x < self.image.width && y < self.image.height;
        if !valid {
            return Err(ImagePlacementError::SourceOutOfBounds {
                x,
                y,
                width,
                height,
                image_width: self.image.width,
                image_height: self.image.height,
            });
        }
        Ok((
            x,
            y,
            width.min(self.image.width - x),
            height.min(self.image.height - y),
        ))
    }
}

/// The protocol-independent decoded result produced by a raw parser.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedGraphics {
    /// Whether the protocol request asks for a response without image display.
    pub query: bool,
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
