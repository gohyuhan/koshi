//! Errors reported while decoding or placing terminal images.

use koshi_core::error::{DomainCategory, DomainError, Severity};
use serde::de::{self, Visitor};
use serde::{Deserialize, Deserializer, Serialize};
use thiserror::Error;

use crate::{ImageDimension, MAX_GRAPHICS_CONTROL_BYTE_COUNT};

struct BoundedGraphicsTextVisitor;

fn validate_graphics_text<E>(text_value: &str) -> Result<(), E>
where
    E: de::Error,
{
    if text_value.len() > MAX_GRAPHICS_CONTROL_BYTE_COUNT {
        return Err(E::custom(format!(
            "graphics error text exceeds {MAX_GRAPHICS_CONTROL_BYTE_COUNT} bytes"
        )));
    }
    Ok(())
}

impl<'de> Visitor<'de> for BoundedGraphicsTextVisitor {
    type Value = String;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("bounded graphics error text")
    }

    fn visit_str<E>(self, text_value: &str) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        validate_graphics_text(text_value)?;
        Ok(text_value.to_owned())
    }

    fn visit_string<E>(self, text_value: String) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        validate_graphics_text(&text_value)?;
        Ok(text_value)
    }
}

fn deserialize_graphics_text<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: Deserializer<'de>,
{
    deserializer.deserialize_string(BoundedGraphicsTextVisitor)
}

/// A recoverable terminal-image processing error.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Error)]
pub enum GraphicsError {
    /// A sequence ended without a complete image transfer.
    #[error("{protocol:?} image transfer is truncated")]
    Truncated { protocol: crate::GraphicsProtocol },
    /// A protocol opening or header is not valid.
    #[error("{protocol:?} image header is invalid")]
    InvalidHeader { protocol: crate::GraphicsProtocol },
    /// A protocol command byte or command parameter is not valid.
    #[error("{protocol:?} image command is invalid")]
    InvalidCommand { protocol: crate::GraphicsProtocol },
    /// Base64 data contains a byte or padding that the protocol does not allow.
    #[error("{protocol:?} image base64 data is invalid")]
    InvalidBase64 { protocol: crate::GraphicsProtocol },
    /// An action such as kitty placement is outside the decoder contract.
    #[error("{protocol:?} image action is unsupported: {action}")]
    UnsupportedAction {
        protocol: crate::GraphicsProtocol,
        #[serde(deserialize_with = "deserialize_graphics_text")]
        action: String,
    },
    /// The encoded media type is not one of the supported raster formats.
    #[error("{protocol:?} image media is unsupported: {media_format}")]
    UnsupportedMedia {
        protocol: crate::GraphicsProtocol,
        #[serde(deserialize_with = "deserialize_graphics_text")]
        #[serde(rename = "format")]
        media_format: String,
    },
    /// A transfer exceeds the encoded-byte bound.
    #[error("{protocol:?} image transfer is too large")]
    TransferTooLarge { protocol: crate::GraphicsProtocol },
    /// A decoded image exceeds the dimension or pixel bound.
    #[error("{protocol:?} image is too large")]
    ImageTooLarge { protocol: crate::GraphicsProtocol },
    /// Width, height, or a byte-count multiplication is invalid.
    #[error("{protocol:?} image dimensions are invalid")]
    InvalidDimensions { protocol: crate::GraphicsProtocol },
    /// A decoded display record cannot become an active image placement.
    #[error("{protocol:?} image placement was rejected: {placement_error}")]
    PlacementRejected {
        /// Protocol that supplied the rejected display record.
        protocol: crate::GraphicsProtocol,
        /// The state validation failure that rejected the placement.
        #[source]
        #[serde(rename = "reason")]
        placement_error: ImagePlacementError,
    },
    /// A sender declared a byte count that does not match its payload.
    #[error(
        "{protocol:?} image declared {expected_byte_count} bytes but carried {actual_byte_count} bytes"
    )]
    DeclaredSizeMismatch {
        protocol: crate::GraphicsProtocol,
        #[serde(rename = "expected")]
        expected_byte_count: usize,
        #[serde(rename = "actual")]
        actual_byte_count: usize,
    },
    /// A multipart iTerm2 command arrived in the wrong order.
    #[error("iTerm2 multipart image state is invalid")]
    MultipartState,
    /// A decoder reported an error or panicked while reading the image.
    #[error("{protocol:?} image data could not be decoded")]
    DecodeFailure { protocol: crate::GraphicsProtocol },
    /// The caller exceeded the graphics event count or image-byte limit.
    #[error(
        "{dropped_event_count} graphics events were dropped because the graphics event count or image-byte limit was reached"
    )]
    QueueFull {
        #[serde(rename = "dropped")]
        dropped_event_count: usize,
    },
}

impl DomainError for GraphicsError {
    /// Image decode failures belong to terminal emulation.
    fn category(&self) -> DomainCategory {
        DomainCategory::Terminal
    }

    /// One rejected image does not stop the pane.
    fn get_severity(&self) -> Severity {
        Severity::Recoverable
    }
}

/// A failure that leaves terminal image state unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Error)]
pub enum ImagePlacementError {
    /// The requested placement mode has no terminal implementation.
    #[error("image placement mode is unsupported")]
    UnsupportedPlacement,
    /// A relative placement names no existing parent placement.
    #[error("relative image placement parent does not exist")]
    ParentNotFound,
    /// A relative placement would make a parent chain cycle.
    #[error("relative image placement would create a cycle")]
    RelativeCycle,
    /// A relative placement chain exceeds the supported depth.
    #[error("relative image placement chain is too deep")]
    RelativeDepth,
    /// A virtual Unicode-placeholder placement cannot itself be relative.
    #[error("a virtual Unicode-placeholder placement cannot be relative")]
    VirtualRelative,
    /// A relative placement offset resolves outside the signed cell range.
    #[error(
        "relative image placement offset resolves to row {resolved_row}, column {resolved_column}"
    )]
    RelativeOffsetOutOfBounds {
        #[serde(rename = "row")]
        resolved_row: i64,
        #[serde(rename = "column")]
        resolved_column: i64,
    },
    /// An animation command names a frame that is not retained.
    #[error("animation frame {frame_index} does not exist")]
    AnimationFrameNotFound {
        #[serde(rename = "frame")]
        frame_index: u32,
    },
    /// An animation command does not fit its retained image canvas.
    #[error("animation command data is invalid")]
    InvalidAnimationData,
    /// No retained Kitty upload matches the requested image identity.
    #[error("no retained Kitty image matches id {image_id:?} or number {image_number:?}")]
    ImageNotFound {
        #[serde(rename = "id")]
        image_id: Option<u32>,
        #[serde(rename = "number")]
        image_number: Option<u32>,
    },
    /// The display request does not contain enough cell-sized information.
    #[error("image dimensions cannot be converted to terminal cells: width {requested_width:?}, height {requested_height:?}")]
    MissingCellDimensions {
        /// The requested width from the image protocol.
        #[serde(rename = "width")]
        requested_width: Option<ImageDimension>,
        /// The requested height from the image protocol.
        #[serde(rename = "height")]
        requested_height: Option<ImageDimension>,
    },
    /// The display request uses a unit that this state model cannot map to cells.
    #[error("image dimensions use unsupported cell units: width {requested_width:?}, height {requested_height:?}")]
    UnsupportedCellDimensions {
        /// The requested width from the image protocol.
        #[serde(rename = "width")]
        requested_width: Option<ImageDimension>,
        /// The requested height from the image protocol.
        #[serde(rename = "height")]
        requested_height: Option<ImageDimension>,
    },
    /// The requested placement has no cells.
    #[error("image placement has zero cells: {column_count} columns by {row_count} rows")]
    ZeroSize {
        #[serde(rename = "columns")]
        column_count: u32,
        #[serde(rename = "rows")]
        row_count: u32,
    },
    /// The Kitty source rectangle is outside the decoded image.
    #[error(
        "image source rectangle at ({source_x},{source_y}) with {source_pixel_width} pixels by {source_pixel_height} pixels exceeds the {image_pixel_width}-pixel by {image_pixel_height}-pixel image"
    )]
    SourceOutOfBounds {
        /// The source rectangle's left edge in pixels.
        #[serde(rename = "x")]
        source_x: u32,
        /// The source rectangle's top edge in pixels.
        #[serde(rename = "y")]
        source_y: u32,
        /// The source rectangle width in pixels.
        #[serde(rename = "width")]
        source_pixel_width: u32,
        /// The source rectangle height in pixels.
        #[serde(rename = "height")]
        source_pixel_height: u32,
        /// The decoded image width in pixels.
        #[serde(rename = "image_width")]
        image_pixel_width: u32,
        /// The decoded image height in pixels.
        #[serde(rename = "image_height")]
        image_pixel_height: u32,
    },
    /// The requested cell dimensions cannot fit in the coordinate type.
    #[error("image placement is too large: {column_count} columns by {row_count} rows")]
    DimensionsTooLarge {
        #[serde(rename = "columns")]
        column_count: u32,
        #[serde(rename = "rows")]
        row_count: u32,
    },
    /// The placement anchor is outside the active grid.
    #[error(
        "image placement at row {anchor_row}, column {anchor_column} with {column_count} columns by {row_count} rows exceeds the {grid_rows}-row by {grid_columns}-column grid"
    )]
    OutOfBounds {
        /// The zero-based row of the placement anchor.
        #[serde(rename = "row")]
        anchor_row: u16,
        /// The zero-based column of the placement anchor.
        #[serde(rename = "column")]
        anchor_column: u16,
        /// The number of covered columns.
        #[serde(rename = "columns")]
        column_count: u16,
        /// The number of covered rows.
        #[serde(rename = "rows")]
        row_count: u16,
        /// The active grid height.
        grid_rows: u16,
        /// The active grid width.
        grid_columns: u16,
    },
    /// The complete placement rectangle is outside the retained primary rows.
    #[error(
        "image placement at primary row {anchor_row}, column {anchor_column} with {column_count} columns by {row_count} rows exceeds retained primary rows {first_row} up to but not including {retained_end}"
    )]
    HistoryOutOfBounds {
        /// The absolute row of the placement anchor.
        #[serde(rename = "row")]
        anchor_row: u64,
        /// The zero-based column of the placement anchor.
        #[serde(rename = "column")]
        anchor_column: u16,
        /// The number of covered columns.
        #[serde(rename = "columns")]
        column_count: u16,
        /// The number of covered rows.
        #[serde(rename = "rows")]
        row_count: u16,
        /// The oldest retained primary row.
        first_row: u64,
        /// The exclusive end of the retained primary row range.
        retained_end: u64,
    },
    /// The complete placement rectangle exceeds the primary grid width.
    #[error(
        "image placement at primary row {anchor_row}, column {anchor_column} with {column_count} columns exceeds the {grid_columns}-column primary grid"
    )]
    HistoryWidthOutOfBounds {
        /// The absolute row of the placement anchor.
        #[serde(rename = "row")]
        anchor_row: u64,
        /// The zero-based column of the placement anchor.
        #[serde(rename = "column")]
        anchor_column: u16,
        /// The number of covered columns.
        #[serde(rename = "columns")]
        column_count: u16,
        /// The active primary grid width.
        grid_columns: u16,
    },
    /// The absolute primary row range cannot represent the live grid boundary.
    #[error("primary image row range at {total_pushed_row_count} with {grid_rows} live rows overflows u64")]
    HistoryRangeOverflow {
        /// The absolute row immediately above the live grid.
        #[serde(rename = "total_pushed")]
        total_pushed_row_count: u64,
        /// The number of rows in the live primary grid.
        grid_rows: u16,
    },
    /// The retained primary rows cannot be assigned nonnegative absolute rows.
    #[error(
        "primary scrollback row count {retained_row_count} exceeds total pushed count {total_pushed_row_count}"
    )]
    HistoryRowsExceedCounter {
        /// The number of retained primary rows.
        #[serde(rename = "retained_rows")]
        retained_row_count: u64,
        /// The absolute row immediately above the live grid.
        #[serde(rename = "total_pushed")]
        total_pushed_row_count: u64,
    },
    /// The terminal-local placement identity cannot advance.
    #[error("image placement identity space is exhausted")]
    IdentityExhausted,
    /// The terminal has reached its placement-count limit.
    #[error("image placement count {placement_count} exceeds the limit of {placement_limit}")]
    TooManyPlacements {
        #[serde(rename = "count")]
        placement_count: usize,
        #[serde(rename = "limit")]
        placement_limit: usize,
    },
    /// The terminal has reached its retained-image-byte limit.
    #[error(
        "image placement storage would reach {used_byte_count} plus {requested_byte_count} bytes, exceeding the {byte_limit}-byte limit"
    )]
    StorageLimit {
        /// Retained RGBA bytes before the requested placement.
        #[serde(rename = "used_bytes")]
        used_byte_count: usize,
        /// RGBA bytes in the requested placement.
        #[serde(rename = "requested_bytes")]
        requested_byte_count: usize,
        /// Maximum retained RGBA bytes across both screens.
        #[serde(rename = "limit_bytes")]
        byte_limit: usize,
    },
}
