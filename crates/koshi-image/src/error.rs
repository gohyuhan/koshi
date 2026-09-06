//! Errors reported while decoding or placing terminal images.

use koshi_core::error::{DomainCategory, DomainError, Severity};
use serde::de::{self, Visitor};
use serde::{Deserialize, Deserializer, Serialize};
use thiserror::Error;

use crate::{ImageDimension, MAX_GRAPHICS_CONTROL_BYTES};

struct BoundedGraphicsTextVisitor;

impl<'de> Visitor<'de> for BoundedGraphicsTextVisitor {
    type Value = String;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("bounded graphics error text")
    }

    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        if value.len() > MAX_GRAPHICS_CONTROL_BYTES {
            return Err(E::custom(format!(
                "graphics error text exceeds {MAX_GRAPHICS_CONTROL_BYTES} bytes"
            )));
        }
        Ok(value.to_owned())
    }

    fn visit_string<E>(self, value: String) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        if value.len() > MAX_GRAPHICS_CONTROL_BYTES {
            return Err(E::custom(format!(
                "graphics error text exceeds {MAX_GRAPHICS_CONTROL_BYTES} bytes"
            )));
        }
        Ok(value)
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
    #[error("{protocol:?} image media is unsupported: {format}")]
    UnsupportedMedia {
        protocol: crate::GraphicsProtocol,
        #[serde(deserialize_with = "deserialize_graphics_text")]
        format: String,
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
    #[error("{protocol:?} image placement was rejected: {reason}")]
    PlacementRejected {
        /// Protocol that supplied the rejected display record.
        protocol: crate::GraphicsProtocol,
        /// The state validation failure that rejected the placement.
        #[source]
        reason: ImagePlacementError,
    },
    /// A sender declared a byte count that does not match its payload.
    #[error("{protocol:?} image declared {expected} bytes but carried {actual} bytes")]
    DeclaredSizeMismatch {
        protocol: crate::GraphicsProtocol,
        expected: usize,
        actual: usize,
    },
    /// A multipart iTerm2 command arrived in the wrong order.
    #[error("iTerm2 multipart image state is invalid")]
    MultipartState,
    /// A decoder reported an error or panicked while reading the image.
    #[error("{protocol:?} image data could not be decoded")]
    DecodeFailure { protocol: crate::GraphicsProtocol },
    /// The caller exceeded the graphics event count or image-byte limit.
    #[error(
        "{dropped} graphics events were dropped because the graphics event count or image-byte limit was reached"
    )]
    QueueFull { dropped: usize },
}

impl DomainError for GraphicsError {
    /// Image decode failures belong to terminal emulation.
    fn category(&self) -> DomainCategory {
        DomainCategory::Terminal
    }

    /// One rejected image does not stop the pane.
    fn severity(&self) -> Severity {
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
    NoParent,
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
    #[error("relative image placement offset resolves to row {row}, column {column}")]
    RelativeOffsetOutOfBounds { row: i64, column: i64 },
    /// An animation command names a frame that is not retained.
    #[error("animation frame {frame} does not exist")]
    AnimationFrameNotFound { frame: u32 },
    /// An animation command does not fit its retained image canvas.
    #[error("animation command data is invalid")]
    AnimationDataInvalid,
    /// No retained Kitty upload matches the requested image identity.
    #[error("no retained Kitty image matches id {id:?} or number {number:?}")]
    ImageNotFound {
        id: Option<u32>,
        number: Option<u32>,
    },
    /// The display request does not contain enough cell-sized information.
    #[error("image dimensions cannot be converted to terminal cells: width {width:?}, height {height:?}")]
    MissingCellDimensions {
        /// The requested width from the image protocol.
        width: Option<ImageDimension>,
        /// The requested height from the image protocol.
        height: Option<ImageDimension>,
    },
    /// The display request uses a unit that this state model cannot map to cells.
    #[error("image dimensions use unsupported cell units: width {width:?}, height {height:?}")]
    UnsupportedCellDimensions {
        /// The requested width from the image protocol.
        width: Option<ImageDimension>,
        /// The requested height from the image protocol.
        height: Option<ImageDimension>,
    },
    /// The requested placement has no cells.
    #[error("image placement has zero cells: {columns} columns by {rows} rows")]
    ZeroSize { columns: u32, rows: u32 },
    /// The Kitty source rectangle is outside the decoded image.
    #[error(
        "image source rectangle at ({x},{y}) with {width} pixels by {height} pixels exceeds the {image_width}-pixel by {image_height}-pixel image"
    )]
    SourceOutOfBounds {
        /// The source rectangle's left edge in pixels.
        x: u32,
        /// The source rectangle's top edge in pixels.
        y: u32,
        /// The source rectangle width in pixels.
        width: u32,
        /// The source rectangle height in pixels.
        height: u32,
        /// The decoded image width in pixels.
        image_width: u32,
        /// The decoded image height in pixels.
        image_height: u32,
    },
    /// The requested cell dimensions cannot fit in the coordinate type.
    #[error("image placement is too large: {columns} columns by {rows} rows")]
    DimensionsTooLarge { columns: u32, rows: u32 },
    /// The placement anchor is outside the active grid.
    #[error(
        "image placement at row {row}, column {column} with {columns} columns by {rows} rows exceeds the {grid_rows}-row by {grid_columns}-column grid"
    )]
    OutOfBounds {
        /// The zero-based row of the placement anchor.
        row: u16,
        /// The zero-based column of the placement anchor.
        column: u16,
        /// The number of covered columns.
        columns: u16,
        /// The number of covered rows.
        rows: u16,
        /// The active grid height.
        grid_rows: u16,
        /// The active grid width.
        grid_columns: u16,
    },
    /// The complete placement rectangle is outside the retained primary rows.
    #[error(
        "image placement at primary row {row}, column {column} with {columns} columns by {rows} rows exceeds retained primary rows {first_row} up to but not including {retained_end}"
    )]
    HistoryOutOfBounds {
        /// The absolute row of the placement anchor.
        row: u64,
        /// The zero-based column of the placement anchor.
        column: u16,
        /// The number of covered columns.
        columns: u16,
        /// The number of covered rows.
        rows: u16,
        /// The oldest retained primary row.
        first_row: u64,
        /// The exclusive end of the retained primary row range.
        retained_end: u64,
    },
    /// The complete placement rectangle exceeds the primary grid width.
    #[error(
        "image placement at primary row {row}, column {column} with {columns} columns exceeds the {grid_columns}-column primary grid"
    )]
    HistoryWidthOutOfBounds {
        /// The absolute row of the placement anchor.
        row: u64,
        /// The zero-based column of the placement anchor.
        column: u16,
        /// The number of covered columns.
        columns: u16,
        /// The active primary grid width.
        grid_columns: u16,
    },
    /// The absolute primary row range cannot represent the live grid boundary.
    #[error("primary image row range at {total_pushed} with {grid_rows} live rows overflows u64")]
    HistoryRangeOverflow {
        /// The absolute row immediately above the live grid.
        total_pushed: u64,
        /// The number of rows in the live primary grid.
        grid_rows: u16,
    },
    /// The retained primary rows cannot be assigned nonnegative absolute rows.
    #[error(
        "primary scrollback row count {retained_rows} exceeds total pushed count {total_pushed}"
    )]
    HistoryRowsExceedCounter {
        /// The number of retained primary rows.
        retained_rows: u64,
        /// The absolute row immediately above the live grid.
        total_pushed: u64,
    },
    /// The terminal-local placement identity cannot advance.
    #[error("image placement identity space is exhausted")]
    IdentityExhausted,
    /// The terminal has reached its placement-count limit.
    #[error("image placement count {count} exceeds the limit of {limit}")]
    TooManyPlacements { count: usize, limit: usize },
    /// The terminal has reached its retained-image-byte limit.
    #[error(
        "image placement storage would reach {used_bytes} plus {requested_bytes} bytes, exceeding the {limit_bytes}-byte limit"
    )]
    StorageLimit {
        /// Retained RGBA bytes before the requested placement.
        used_bytes: usize,
        /// RGBA bytes in the requested placement.
        requested_bytes: usize,
        /// Maximum retained RGBA bytes across both screens.
        limit_bytes: usize,
    },
}
