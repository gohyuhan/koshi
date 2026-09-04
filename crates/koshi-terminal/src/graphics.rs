//! Bounded decoding for terminal image escape sequences.
//!
//! The decoder accepts Sixel, kitty graphics, and iTerm2 inline image
//! transfers. It turns each complete image into RGBA pixels and never writes
//! to a terminal grid. The terminal engine adds the cursor position at which
//! the sequence ended, then applies display records to terminal image state.

use std::io::{Cursor, Read};
use std::panic::{catch_unwind, AssertUnwindSafe};

use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD};
use base64::Engine;
use flate2::bufread::ZlibDecoder;
use koshi_core::error::{DomainCategory, DomainError, Severity};
use serde::de::{self, DeserializeSeed, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};
use thiserror::Error;

use crate::state::ImagePlacementError;

/// The largest decoded image, measured in pixels.
pub const MAX_IMAGE_PIXELS: usize = 16_777_216;

/// The largest decoded RGBA buffer, measured in bytes.
pub const MAX_IMAGE_BYTES: usize = MAX_IMAGE_PIXELS * 4;

/// The largest encoded transfer held by one graphics sequence.
pub const MAX_GRAPHICS_TRANSFER_BYTES: usize = 32 * 1024 * 1024;

/// The largest protocol header or command held while it is parsed.
pub const MAX_GRAPHICS_CONTROL_BYTES: usize = 8 * 1024;

/// The largest image side accepted by a decoder.
pub const MAX_IMAGE_SIDE: usize = 16_384;

/// The largest raw graphics prefix carried between parser instances.
pub const MAX_GRAPHICS_CARRY_BYTES: usize = 64 * 1024;

/// The largest GNU Screen passthrough body accepted by this parser.
const MAX_SCREEN_PASSTHROUGH_BYTES: usize = 768;

/// The largest base64 chunk accepted by the kitty graphics protocol.
const MAX_KITTY_CHUNK_BYTES: usize = 4096;

/// The deepest tmux or GNU Screen wrapper accepted around one image stream.
const MAX_GRAPHICS_WRAPPER_DEPTH: usize = 8;

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
            cell_columns: None,
            cell_rows: None,
            source_offset_x: None,
            source_offset_y: None,
            cell_offset_x: None,
            cell_offset_y: None,
            move_cursor: true,
        }
    }
}

/// A validated row-major RGBA image.
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
    pub image: DecodedImage,
    /// The state operation represented by the transfer.
    pub action: ImageAction,
    /// Display hints supplied by the protocol.
    pub display: ImageDisplay,
    /// Cursor position when the image sequence ended, as row and column.
    pub anchor: (u16, u16),
}

impl ImageRecord {
    /// Return the source rectangle used by a Kitty image placement.
    ///
    /// The tuple is `(x, y, width, height)` in decoded-image pixels. Kitty
    /// `x` and `y` select the source origin; `w` and `h` are represented by
    /// pixel dimensions in `display`. Other protocols use the complete image.
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
        let valid = width > 0
            && height > 0
            && x.checked_add(width)
                .is_some_and(|end| end <= self.image.width)
            && y.checked_add(height)
                .is_some_and(|end| end <= self.image.height);
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
        Ok((x, y, width, height))
    }
}

/// Why a graphics parser cannot rebuild all of its active bytes after a
/// terminal-engine replacement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum GraphicsAbandonment {
    /// Consume the remainder of one open protocol string and report its limit.
    Sequence(GraphicsProtocol),
    /// Consume the remainder of one ignored protocol string without reporting
    /// a graphics event.
    SilentSequence(GraphicsProtocol),
    /// Consume following multipart records until their final record and report the
    /// limit once that record arrives.
    Transfer(GraphicsProtocol),
}

/// Parser state carried across a terminal-engine replacement.
///
/// `carry` rebuilds this parser's own active sequence or multipart transfer.
/// The two nested records rebuild parsers inside a split GNU Screen or tmux
/// wrapper. A one-pixel red iTerm2 transfer split after `ESC ] 1337;File` has
/// the outer wrapper in one record and the unfinished iTerm2 command in its
/// nested record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GraphicsTransportState {
    /// Bytes that rebuild this parser's own active sequence or transfer.
    #[serde(default)]
    pub carry: Vec<u8>,
    /// Whether [`carry`](Self::carry) contains a complete bounded rebuild.
    /// `false` means [`abandonment`](Self::abandonment) describes how to drain
    /// the open transfer after restore.
    #[serde(default = "default_true")]
    pub carryable: bool,
    /// How to drain an open sequence when `carryable` is false.
    #[serde(default)]
    pub abandonment: Option<GraphicsAbandonment>,
    /// Whether the next DCS is a GNU Screen continuation wrapper.
    #[serde(default)]
    pub screen_continuation: bool,
    /// Whether the carried bytes are inside an open GNU Screen wrapper.
    #[serde(default)]
    pub screen_wrapper_active: bool,
    /// The parser state inside the carried GNU Screen wrapper, when that
    /// wrapper ended while its enclosed stream was incomplete.
    #[serde(default)]
    pub screen_inner: Option<Box<GraphicsTransportState>>,
    /// Whether the next DCS is a tmux continuation wrapper.
    #[serde(default)]
    pub tmux_continuation: bool,
    /// Whether the carried bytes are inside an open tmux wrapper.
    #[serde(default)]
    pub tmux_wrapper_active: bool,
    /// The parser state inside the carried tmux wrapper, when that wrapper
    /// ended while its enclosed stream was incomplete.
    #[serde(default)]
    pub tmux_inner: Option<Box<GraphicsTransportState>>,
}

impl Default for GraphicsTransportState {
    fn default() -> Self {
        GraphicsTransportState {
            carry: Vec::new(),
            carryable: true,
            abandonment: None,
            screen_continuation: false,
            screen_wrapper_active: false,
            screen_inner: None,
            tmux_continuation: false,
            tmux_wrapper_active: false,
            tmux_inner: None,
        }
    }
}

impl<'de> Deserialize<'de> for GraphicsTransportState {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_map(GraphicsTransportVisitor { depth: 0 })
    }
}

struct GraphicsTransportVisitor {
    depth: usize,
}

impl<'de> Visitor<'de> for GraphicsTransportVisitor {
    type Value = GraphicsTransportState;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("a graphics transport state object")
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut carry = None;
        let mut carryable = None;
        let mut abandonment = None;
        let mut screen_continuation = None;
        let mut screen_wrapper_active = None;
        let mut screen_inner = None;
        let mut tmux_continuation = None;
        let mut tmux_wrapper_active = None;
        let mut tmux_inner = None;

        while let Some(key) = map.next_key::<String>()? {
            match key.as_str() {
                "carry" => {
                    if carry.is_some() {
                        return Err(de::Error::duplicate_field("carry"));
                    }
                    carry = Some(map.next_value_seed(BoundedBytesSeed {
                        limit: MAX_GRAPHICS_CARRY_BYTES,
                        name: "graphics carry",
                    })?);
                }
                "carryable" => {
                    if carryable.is_some() {
                        return Err(de::Error::duplicate_field("carryable"));
                    }
                    carryable = Some(map.next_value()?);
                }
                "abandonment" => {
                    if abandonment.is_some() {
                        return Err(de::Error::duplicate_field("abandonment"));
                    }
                    abandonment = Some(map.next_value()?);
                }
                "screen_continuation" => {
                    if screen_continuation.is_some() {
                        return Err(de::Error::duplicate_field("screen_continuation"));
                    }
                    screen_continuation = Some(map.next_value()?);
                }
                "screen_wrapper_active" => {
                    if screen_wrapper_active.is_some() {
                        return Err(de::Error::duplicate_field("screen_wrapper_active"));
                    }
                    screen_wrapper_active = Some(map.next_value()?);
                }
                "screen_inner" => {
                    if screen_inner.is_some() {
                        return Err(de::Error::duplicate_field("screen_inner"));
                    }
                    screen_inner = Some(map.next_value_seed(GraphicsTransportOptionSeed {
                        depth: self.depth.saturating_add(1),
                    })?);
                }
                "tmux_continuation" => {
                    if tmux_continuation.is_some() {
                        return Err(de::Error::duplicate_field("tmux_continuation"));
                    }
                    tmux_continuation = Some(map.next_value()?);
                }
                "tmux_wrapper_active" => {
                    if tmux_wrapper_active.is_some() {
                        return Err(de::Error::duplicate_field("tmux_wrapper_active"));
                    }
                    tmux_wrapper_active = Some(map.next_value()?);
                }
                "tmux_inner" => {
                    if tmux_inner.is_some() {
                        return Err(de::Error::duplicate_field("tmux_inner"));
                    }
                    tmux_inner = Some(map.next_value_seed(GraphicsTransportOptionSeed {
                        depth: self.depth.saturating_add(1),
                    })?);
                }
                _ => {
                    let _: de::IgnoredAny = map.next_value()?;
                }
            }
        }

        Ok(GraphicsTransportState {
            carry: carry.unwrap_or_default(),
            carryable: carryable.unwrap_or_else(default_true),
            abandonment: abandonment.unwrap_or_default(),
            screen_continuation: screen_continuation.unwrap_or(false),
            screen_wrapper_active: screen_wrapper_active.unwrap_or(false),
            screen_inner: screen_inner.unwrap_or_default(),
            tmux_continuation: tmux_continuation.unwrap_or(false),
            tmux_wrapper_active: tmux_wrapper_active.unwrap_or(false),
            tmux_inner: tmux_inner.unwrap_or_default(),
        })
    }
}

struct GraphicsTransportOptionSeed {
    depth: usize,
}

impl<'de> DeserializeSeed<'de> for GraphicsTransportOptionSeed {
    type Value = Option<Box<GraphicsTransportState>>;

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_option(GraphicsTransportOptionVisitor { depth: self.depth })
    }
}

struct GraphicsTransportOptionVisitor {
    depth: usize,
}

impl<'de> Visitor<'de> for GraphicsTransportOptionVisitor {
    type Value = Option<Box<GraphicsTransportState>>;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("null or a nested graphics transport state object")
    }

    fn visit_none<E>(self) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(None)
    }

    fn visit_some<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        if self.depth > MAX_GRAPHICS_WRAPPER_DEPTH {
            return Err(de::Error::custom(
                "graphics wrapper nesting exceeds the supported limit",
            ));
        }
        let state = deserializer.deserialize_map(GraphicsTransportVisitor { depth: self.depth })?;
        Ok(Some(Box::new(state)))
    }
}

struct BoundedBytesSeed {
    limit: usize,
    name: &'static str,
}

impl<'de> DeserializeSeed<'de> for BoundedBytesSeed {
    type Value = Vec<u8>;

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_seq(BoundedBytesVisitor {
            limit: self.limit,
            name: self.name,
        })
    }
}

struct BoundedBytesVisitor {
    limit: usize,
    name: &'static str,
}

impl<'de> Visitor<'de> for BoundedBytesVisitor {
    type Value = Vec<u8>;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("a bounded byte sequence")
    }

    fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        let mut bytes = Vec::with_capacity(sequence.size_hint().unwrap_or(0).min(self.limit));
        while let Some(byte) = sequence.next_element::<u8>()? {
            if bytes.len() == self.limit {
                return Err(de::Error::custom(format!(
                    "{name} exceeds {limit} bytes",
                    name = self.name,
                    limit = self.limit,
                )));
            }
            bytes.push(byte);
        }
        Ok(bytes)
    }

    fn visit_bytes<E>(self, bytes: &[u8]) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        if bytes.len() > self.limit {
            return Err(E::custom(format!(
                "{name} exceeds {limit} bytes",
                name = self.name,
                limit = self.limit,
            )));
        }
        Ok(bytes.to_vec())
    }

    fn visit_byte_buf<E>(self, bytes: Vec<u8>) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        if bytes.len() > self.limit {
            return Err(E::custom(format!(
                "{name} exceeds {limit} bytes",
                name = self.name,
                limit = self.limit,
            )));
        }
        Ok(bytes)
    }
}

fn deserialize_graphics_text<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: Deserializer<'de>,
{
    deserializer.deserialize_string(BoundedGraphicsTextVisitor)
}

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

fn deserialize_rgba<'de, D>(deserializer: D) -> Result<Vec<u8>, D::Error>
where
    D: Deserializer<'de>,
{
    BoundedBytesSeed {
        limit: MAX_IMAGE_BYTES,
        name: "decoded image RGBA data",
    }
    .deserialize(deserializer)
}

fn default_true() -> bool {
    true
}

/// A recoverable terminal-image processing error.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Error)]
pub enum GraphicsError {
    /// A sequence ended without a complete image transfer.
    #[error("{protocol:?} image transfer is truncated")]
    Truncated { protocol: GraphicsProtocol },
    /// A protocol opening or header is not valid.
    #[error("{protocol:?} image header is invalid")]
    InvalidHeader { protocol: GraphicsProtocol },
    /// A protocol command byte or command parameter is not valid.
    #[error("{protocol:?} image command is invalid")]
    InvalidCommand { protocol: GraphicsProtocol },
    /// Base64 data contains a byte or padding that the protocol does not allow.
    #[error("{protocol:?} image base64 data is invalid")]
    InvalidBase64 { protocol: GraphicsProtocol },
    /// An action such as kitty placement is outside the decoder contract.
    #[error("{protocol:?} image action is unsupported: {action}")]
    UnsupportedAction {
        protocol: GraphicsProtocol,
        #[serde(deserialize_with = "deserialize_graphics_text")]
        action: String,
    },
    /// The encoded media type is not one of the supported raster formats.
    #[error("{protocol:?} image media is unsupported: {format}")]
    UnsupportedMedia {
        protocol: GraphicsProtocol,
        #[serde(deserialize_with = "deserialize_graphics_text")]
        format: String,
    },
    /// A transfer exceeds the encoded-byte bound.
    #[error("{protocol:?} image transfer is too large")]
    TransferTooLarge { protocol: GraphicsProtocol },
    /// A decoded image exceeds the dimension or pixel bound.
    #[error("{protocol:?} image is too large")]
    ImageTooLarge { protocol: GraphicsProtocol },
    /// Width, height, or a byte-count multiplication is invalid.
    #[error("{protocol:?} image dimensions are invalid")]
    InvalidDimensions { protocol: GraphicsProtocol },
    /// A decoded display record cannot become an active image placement.
    #[error("{protocol:?} image placement was rejected: {reason}")]
    PlacementRejected {
        /// Protocol that supplied the rejected display record.
        protocol: GraphicsProtocol,
        /// The state validation failure that rejected the placement.
        #[source]
        reason: ImagePlacementError,
    },
    /// A sender declared a byte count that does not match its payload.
    #[error("{protocol:?} image declared {expected} bytes but carried {actual} bytes")]
    DeclaredSizeMismatch {
        protocol: GraphicsProtocol,
        expected: usize,
        actual: usize,
    },
    /// A multipart iTerm2 command arrived in the wrong order.
    #[error("iTerm2 multipart image state is invalid")]
    MultipartState,
    /// A decoder reported an error or panicked while reading the image.
    #[error("{protocol:?} image data could not be decoded")]
    DecodeFailure { protocol: GraphicsProtocol },
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

/// The protocol-independent decoded result produced by the raw parser.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DecodedGraphics {
    /// Protocol that supplied the image.
    pub protocol: GraphicsProtocol,
    /// Validated image pixels.
    pub image: DecodedImage,
    /// The state operation represented by the transfer.
    pub action: ImageAction,
    /// Display hints from the transfer.
    pub display: ImageDisplay,
}

/// Raw protocol decoder owned by the terminal engine.
#[derive(Clone)]
pub(crate) struct GraphicsParser {
    state: GraphicsState,
    pending: Vec<u8>,
    carryable: bool,
    sequence_bytes: usize,
    wrapper_depth: usize,
    transfer_carry: Vec<u8>,
    transfer_carryable: bool,
    abandoned_transfer: Option<GraphicsProtocol>,
    screen_continuation: bool,
    screen_inner: Option<Box<GraphicsParser>>,
    tmux_continuation: bool,
    tmux_inner: Option<Box<GraphicsParser>>,
    kitty_transfer: Option<KittyTransfer>,
    iterm_transfer: Option<ItermTransfer>,
}

impl Default for GraphicsParser {
    fn default() -> Self {
        GraphicsParser {
            state: GraphicsState::default(),
            pending: Vec::new(),
            carryable: true,
            sequence_bytes: 0,
            wrapper_depth: 0,
            transfer_carry: Vec::new(),
            transfer_carryable: true,
            abandoned_transfer: None,
            screen_continuation: false,
            screen_inner: None,
            tmux_continuation: false,
            tmux_inner: None,
            kitty_transfer: None,
            iterm_transfer: None,
        }
    }
}

#[derive(Clone, Default)]
enum GraphicsState {
    #[default]
    Ground,
    Escape,
    DcsIntro,
    Sixel(Box<SixelParser>),
    Kitty(KittyParser),
    Iterm(ItermParser),
    Tmux(TmuxParser),
    Screen(ScreenParser),
    Discard(DiscardParser),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StringKind {
    Dcs,
    Apc,
    Osc,
}

impl StringKind {
    fn protocol(self) -> GraphicsProtocol {
        match self {
            StringKind::Dcs => GraphicsProtocol::Sixel,
            StringKind::Apc => GraphicsProtocol::Kitty,
            StringKind::Osc => GraphicsProtocol::Iterm2,
        }
    }
}

impl GraphicsProtocol {
    fn string_kind(self) -> StringKind {
        match self {
            GraphicsProtocol::Sixel => StringKind::Dcs,
            GraphicsProtocol::Kitty => StringKind::Apc,
            GraphicsProtocol::Iterm2 => StringKind::Osc,
        }
    }
}

#[derive(Clone)]
struct DiscardParser {
    kind: StringKind,
    error: GraphicsError,
    escaped: bool,
    report: bool,
}

impl GraphicsParser {
    /// Feed bytes and return every image or error completed by this chunk.
    pub(crate) fn advance(&mut self, bytes: &[u8]) -> Vec<Result<DecodedGraphics, GraphicsError>> {
        let mut events = Vec::new();
        for &byte in bytes {
            self.feed_byte(byte, &mut events);
        }
        events
    }

    pub(crate) fn advance_with_offsets(
        &mut self,
        bytes: &[u8],
    ) -> Vec<(usize, Result<DecodedGraphics, GraphicsError>)> {
        let mut events = Vec::new();
        let mut byte_events = Vec::new();
        for (offset, &byte) in bytes.iter().enumerate() {
            byte_events.clear();
            self.feed_byte(byte, &mut byte_events);
            events.extend(byte_events.drain(..).map(|event| (offset, event)));
        }
        events
    }

    /// Return bytes needed to rebuild an active graphics parser after a
    /// process-image swap. An empty slice means that the active transfer
    /// exceeded the carry bound and must not be resumed from its opening.
    pub(crate) fn carry_bytes(&self) -> Option<&[u8]> {
        if matches!(self.state, GraphicsState::Ground) {
            if let Some(inner) = &self.screen_inner {
                return inner.carry_bytes();
            }
            if let Some(inner) = &self.tmux_inner {
                return inner.carry_bytes();
            }
        }
        if matches!(self.state, GraphicsState::Ground) {
            if self.has_open_transfer() && self.transfer_carryable {
                Some(&self.transfer_carry)
            } else if self.has_open_transfer() {
                Some(&[])
            } else {
                None
            }
        } else if self.carryable {
            Some(&self.pending)
        } else {
            Some(&[])
        }
    }

    pub(crate) fn transport_state(&self) -> Option<GraphicsTransportState> {
        self.has_pending_state().then(|| self.transport_snapshot())
    }

    fn transport_snapshot(&self) -> GraphicsTransportState {
        let (carry, carryable) = if matches!(self.state, GraphicsState::Ground) {
            if self.has_own_transfer() {
                (self.transfer_carry.clone(), self.transfer_carryable)
            } else {
                (Vec::new(), true)
            }
        } else {
            (self.pending.clone(), self.carryable)
        };
        GraphicsTransportState {
            carry,
            carryable,
            abandonment: self.abandonment(),
            screen_continuation: self.screen_continuation,
            screen_wrapper_active: self.screen_wrapper_active(),
            screen_inner: self
                .screen_inner
                .as_ref()
                .map(|inner| Box::new(inner.transport_snapshot())),
            tmux_continuation: self.tmux_continuation,
            tmux_wrapper_active: self.tmux_wrapper_active(),
            tmux_inner: self
                .tmux_inner
                .as_ref()
                .map(|inner| Box::new(inner.transport_snapshot())),
        }
    }

    /// Return whether a multipart transfer has bytes that still need a final
    /// protocol record.
    pub(crate) fn has_open_transfer(&self) -> bool {
        self.kitty_transfer.is_some()
            || self.iterm_transfer.is_some()
            || self
                .screen_inner
                .as_ref()
                .is_some_and(|inner| inner.has_open_transfer())
            || self
                .tmux_inner
                .as_ref()
                .is_some_and(|inner| inner.has_open_transfer())
    }

    fn has_own_transfer(&self) -> bool {
        self.kitty_transfer.is_some() || self.iterm_transfer.is_some()
    }

    fn state_protocol(&self) -> GraphicsProtocol {
        match &self.state {
            GraphicsState::Kitty(_) => GraphicsProtocol::Kitty,
            GraphicsState::Iterm(_) => GraphicsProtocol::Iterm2,
            GraphicsState::Discard(parser) => parser.kind.protocol(),
            _ => GraphicsProtocol::Sixel,
        }
    }

    fn abandonment(&self) -> Option<GraphicsAbandonment> {
        if let Some(protocol) = self.abandoned_transfer {
            return Some(GraphicsAbandonment::Transfer(protocol));
        }
        let carryable = if matches!(self.state, GraphicsState::Ground) && self.has_own_transfer() {
            self.transfer_carryable
        } else {
            self.carryable
        };
        if carryable {
            return None;
        }
        if self.has_own_transfer() {
            let protocol = if self.kitty_transfer.is_some() {
                GraphicsProtocol::Kitty
            } else {
                GraphicsProtocol::Iterm2
            };
            Some(GraphicsAbandonment::Transfer(protocol))
        } else {
            let protocol = self.state_protocol();
            if self.reports_active_sequence() {
                Some(GraphicsAbandonment::Sequence(protocol))
            } else {
                Some(GraphicsAbandonment::SilentSequence(protocol))
            }
        }
    }

    fn has_pending_state(&self) -> bool {
        !matches!(self.state, GraphicsState::Ground)
            || self.abandoned_transfer.is_some()
            || self.screen_continuation
            || self.screen_inner.is_some()
            || self.tmux_continuation
            || self.tmux_inner.is_some()
            || self.has_open_transfer()
    }

    /// Return whether the next DCS is a GNU Screen continuation wrapper.
    pub(crate) fn screen_continuation(&self) -> bool {
        self.screen_continuation
    }

    pub(crate) fn screen_wrapper_active(&self) -> bool {
        self.screen_continuation
            && matches!(
                self.state,
                GraphicsState::DcsIntro | GraphicsState::Screen(_)
            )
    }

    pub(crate) fn tmux_continuation(&self) -> bool {
        self.tmux_continuation
    }

    pub(crate) fn tmux_wrapper_active(&self) -> bool {
        self.tmux_continuation
            && matches!(self.state, GraphicsState::DcsIntro | GraphicsState::Tmux(_))
    }

    pub(crate) fn restore_carry(&mut self, bytes: &[u8], transport: GraphicsTransportState) {
        self.restore_transport(transport, Some(bytes));
    }

    fn restore_transport(
        &mut self,
        transport: GraphicsTransportState,
        top_level_bytes: Option<&[u8]>,
    ) {
        self.screen_continuation = transport.screen_continuation;
        self.tmux_continuation = transport.tmux_continuation;
        self.screen_inner = transport.screen_inner.map(|inner| {
            Box::new(GraphicsParser::from_transport(
                *inner,
                self.wrapper_depth.saturating_add(1),
            ))
        });
        self.tmux_inner = transport.tmux_inner.map(|inner| {
            Box::new(GraphicsParser::from_transport(
                *inner,
                self.wrapper_depth.saturating_add(1),
            ))
        });

        self.abandoned_transfer = match transport.abandonment {
            Some(GraphicsAbandonment::Transfer(protocol)) => Some(protocol),
            Some(GraphicsAbandonment::Sequence(_))
            | Some(GraphicsAbandonment::SilentSequence(_))
            | None => None,
        };

        if let Some(GraphicsAbandonment::Sequence(protocol)) = transport.abandonment {
            self.state = GraphicsState::Discard(DiscardParser {
                kind: protocol.string_kind(),
                error: GraphicsError::TransferTooLarge { protocol },
                escaped: false,
                report: true,
            });
            self.pending.clear();
            self.carryable = false;
            self.sequence_bytes = 0;
            return;
        }
        if let Some(GraphicsAbandonment::SilentSequence(protocol)) = transport.abandonment {
            self.state = GraphicsState::Discard(DiscardParser {
                kind: protocol.string_kind(),
                error: GraphicsError::TransferTooLarge { protocol },
                escaped: false,
                report: false,
            });
            self.pending.clear();
            self.carryable = false;
            self.sequence_bytes = 0;
            return;
        }

        let provided_bytes = top_level_bytes
            .filter(|bytes| !bytes.is_empty())
            .unwrap_or(&transport.carry);
        if self.screen_continuation
            && !transport.screen_wrapper_active
            && self.screen_inner.is_none()
            && !provided_bytes.is_empty()
        {
            let mut inner = GraphicsParser {
                wrapper_depth: self.wrapper_depth.saturating_add(1),
                ..GraphicsParser::default()
            };
            let _ = inner.advance(provided_bytes);
            self.screen_inner = Some(Box::new(inner));
            return;
        }
        if self.tmux_continuation
            && !transport.tmux_wrapper_active
            && self.tmux_inner.is_none()
            && !provided_bytes.is_empty()
        {
            let mut inner = GraphicsParser {
                wrapper_depth: self.wrapper_depth.saturating_add(1),
                ..GraphicsParser::default()
            };
            let _ = inner.advance(provided_bytes);
            self.tmux_inner = Some(Box::new(inner));
            return;
        }
        let bytes = if self.screen_inner.is_some() || self.tmux_inner.is_some() {
            transport.carry.as_slice()
        } else {
            provided_bytes
        };
        if transport.carryable {
            let _ = self.advance(bytes);
        }
    }

    fn from_transport(transport: GraphicsTransportState, wrapper_depth: usize) -> Self {
        let mut parser = GraphicsParser {
            wrapper_depth,
            ..GraphicsParser::default()
        };
        parser.restore_transport(transport, None);
        parser
    }

    /// Finish a stream and report any active sequence or multipart transfer.
    pub(crate) fn finish(&mut self) -> Vec<Result<DecodedGraphics, GraphicsError>> {
        let mut events = Vec::new();
        let active_error = match &self.state {
            GraphicsState::Discard(parser) => Some(parser.error.clone()),
            _ => None,
        };
        let active_protocol = if self.reports_active_sequence() {
            Some(self.state_protocol())
        } else {
            None
        };
        if let Some(protocol) = active_protocol {
            let report = !matches!(
                &self.state,
                GraphicsState::Discard(parser) if !parser.report
            );
            self.reset();
            if report && self.abandoned_transfer != Some(protocol) {
                events.push(Err(
                    active_error.unwrap_or(GraphicsError::Truncated { protocol })
                ));
            }
        } else if !matches!(self.state, GraphicsState::Ground) {
            self.reset();
        }
        if self.kitty_transfer.take().is_some() && active_protocol != Some(GraphicsProtocol::Kitty)
        {
            events.push(Err(GraphicsError::Truncated {
                protocol: GraphicsProtocol::Kitty,
            }));
        }
        if self.iterm_transfer.take().is_some() && active_protocol != Some(GraphicsProtocol::Iterm2)
        {
            events.push(Err(GraphicsError::Truncated {
                protocol: GraphicsProtocol::Iterm2,
            }));
        }
        if let Some(protocol) = self.abandoned_transfer.take() {
            events.push(Err(GraphicsError::TransferTooLarge { protocol }));
        }
        if let Some(mut inner) = self.screen_inner.take() {
            events.extend(inner.finish());
        }
        if let Some(mut inner) = self.tmux_inner.take() {
            events.extend(inner.finish());
        }
        self.transfer_carry.clear();
        self.transfer_carryable = true;
        self.screen_continuation = false;
        self.tmux_continuation = false;
        self.screen_inner = None;
        self.tmux_inner = None;
        events
    }

    fn reports_active_sequence(&self) -> bool {
        match &self.state {
            GraphicsState::Ground | GraphicsState::Escape | GraphicsState::DcsIntro => false,
            GraphicsState::Kitty(parser) => {
                !parser.ignored && parser.header.first().copied() == Some(b'G')
            }
            GraphicsState::Iterm(parser) => {
                !parser.ignored
                    && (parser.prefix_done || parser.prefix.as_slice() == b"1337")
                    && iterm_command_is_graphics(&parser.body)
            }
            GraphicsState::Sixel(parser) => parser.phase == SixelPhase::Body,
            GraphicsState::Tmux(parser) => {
                parser.prefix.len() >= b"tmux;".len()
                    && self.wrapper_contains_graphics(self.tmux_inner.as_deref(), &parser.data)
            }
            GraphicsState::Screen(parser) => {
                self.wrapper_contains_graphics(self.screen_inner.as_deref(), &parser.data)
            }
            GraphicsState::Discard(parser) => parser.report,
        }
    }

    fn wrapper_contains_graphics(&self, inner: Option<&GraphicsParser>, data: &[u8]) -> bool {
        let mut parser = inner.cloned().unwrap_or_default();
        let events = parser.advance(data);
        !events.is_empty() || parser.has_graphics_state()
    }

    fn has_graphics_state(&self) -> bool {
        self.reports_active_sequence()
            || self.has_open_transfer()
            || self
                .screen_inner
                .as_ref()
                .is_some_and(|inner| inner.has_graphics_state())
            || self
                .tmux_inner
                .as_ref()
                .is_some_and(|inner| inner.has_graphics_state())
    }

    fn feed_byte(&mut self, byte: u8, events: &mut Vec<Result<DecodedGraphics, GraphicsError>>) {
        if !matches!(self.state, GraphicsState::Ground) {
            self.push_pending(byte);
            if self.sequence_bytes == MAX_GRAPHICS_TRANSFER_BYTES
                && !matches!(self.state, GraphicsState::Discard(_))
            {
                let kind = match &self.state {
                    GraphicsState::Kitty(_) => StringKind::Apc,
                    GraphicsState::Iterm(_) => StringKind::Osc,
                    _ => StringKind::Dcs,
                };
                self.discard(
                    kind,
                    GraphicsError::TransferTooLarge {
                        protocol: kind.protocol(),
                    },
                    byte,
                );
                return;
            }
            self.sequence_bytes = self.sequence_bytes.saturating_add(1);
        }

        match std::mem::take(&mut self.state) {
            GraphicsState::Ground => {
                if byte == 0x18 || byte == 0x1a {
                    self.cancel_transfers();
                } else if byte == 0x1b {
                    self.begin(GraphicsState::Escape, byte);
                } else if byte == 0x90 {
                    self.begin(GraphicsState::DcsIntro, byte);
                } else if matches!(byte, 0x98 | 0x9e) {
                    self.begin_silent_string(byte);
                } else if byte == 0x9f {
                    self.begin_transfer(GraphicsState::Kitty(KittyParser::new()), byte);
                } else if byte == 0x9d {
                    self.begin_transfer(GraphicsState::Iterm(ItermParser::new()), byte);
                }
            }
            GraphicsState::Escape => self.feed_escape(byte),
            GraphicsState::DcsIntro => self.feed_dcs_intro(byte, events),
            GraphicsState::Sixel(parser) => self.feed_sixel(parser, byte, events),
            GraphicsState::Kitty(parser) => self.feed_kitty(parser, byte, events),
            GraphicsState::Iterm(parser) => self.feed_iterm(parser, byte, events),
            GraphicsState::Tmux(parser) => self.feed_tmux(parser, byte, events),
            GraphicsState::Screen(parser) => self.feed_screen(parser, byte, events),
            GraphicsState::Discard(parser) => self.feed_discard(parser, byte, events),
        }
    }

    fn feed_escape(&mut self, byte: u8) {
        if byte == 0x18 || byte == 0x1a {
            self.cancel_transfers();
            self.reset();
            return;
        }
        match byte {
            b'P' => self.state = GraphicsState::DcsIntro,
            b'_' => self.continue_transfer(GraphicsState::Kitty(KittyParser::new())),
            b']' => self.continue_transfer(GraphicsState::Iterm(ItermParser::new())),
            b'X' | b'^' => self.ignore_string(StringKind::Dcs, byte),
            0x1b => {
                self.pending.clear();
                self.carryable = true;
                self.pending.push(byte);
                self.state = GraphicsState::Escape;
            }
            _ => self.reset(),
        }
    }

    fn feed_dcs_intro(
        &mut self,
        byte: u8,
        events: &mut Vec<Result<DecodedGraphics, GraphicsError>>,
    ) {
        if byte == 0x18 || byte == 0x1a {
            self.cancel_transfers();
            self.reset();
            return;
        }
        if self.screen_continuation {
            if byte == 0x9c {
                self.finish_screen(Vec::new(), events);
            } else {
                self.state = GraphicsState::Screen(ScreenParser::from_first(byte));
            }
            return;
        }
        if self.tmux_continuation {
            if byte == 0x9c {
                self.finish_tmux(Vec::new(), events);
            } else {
                self.state = GraphicsState::Tmux(TmuxParser::from_first(byte));
            }
            return;
        }
        if byte == 0x9c {
            self.reset();
            return;
        }
        if self.wrapper_depth >= MAX_GRAPHICS_WRAPPER_DEPTH && matches!(byte, b't' | 0x1b) {
            self.discard(
                StringKind::Dcs,
                GraphicsError::TransferTooLarge {
                    protocol: GraphicsProtocol::Sixel,
                },
                byte,
            );
            return;
        }
        match byte {
            b'q' => {
                let mut parser = SixelParser::new();
                if parser.feed(b'q').is_err() {
                    self.reset();
                } else {
                    self.state = GraphicsState::Sixel(Box::new(parser));
                }
            }
            b't' => self.state = GraphicsState::Tmux(TmuxParser::new()),
            0x1b => self.state = GraphicsState::Screen(ScreenParser::new()),
            b'0'..=b'9' | b';' => {
                let mut parser = SixelParser::new();
                if parser.feed(byte).is_err() {
                    self.reset();
                } else {
                    self.state = GraphicsState::Sixel(Box::new(parser));
                }
            }
            _ => self.ignore_string(StringKind::Dcs, byte),
        }
    }

    fn feed_sixel(
        &mut self,
        mut parser: Box<SixelParser>,
        byte: u8,
        events: &mut Vec<Result<DecodedGraphics, GraphicsError>>,
    ) {
        if parser.escaped {
            if byte == 0x18 || byte == 0x1a {
                self.cancel_transfers();
                self.reset();
            } else if byte == b'\\' {
                parser.escaped = false;
                self.finish_state((*parser).finish(), events);
            } else {
                self.discard(
                    StringKind::Dcs,
                    GraphicsError::InvalidCommand {
                        protocol: GraphicsProtocol::Sixel,
                    },
                    byte,
                );
            }
            return;
        }
        if byte == 0x18 || byte == 0x1a {
            self.cancel_transfers();
            self.reset();
        } else if byte == 0x1b {
            parser.escaped = true;
            self.state = GraphicsState::Sixel(parser);
        } else if byte == 0x9c {
            self.finish_state(parser.finish(), events);
        } else if let Err(error) = parser.feed(byte) {
            if parser.phase == SixelPhase::Header
                && byte != b'q'
                && !matches!(error, GraphicsError::TransferTooLarge { .. })
            {
                self.ignore_string(StringKind::Dcs, byte);
            } else {
                self.discard(StringKind::Dcs, error, byte);
            }
        } else {
            self.state = GraphicsState::Sixel(parser);
        }
    }

    fn feed_kitty(
        &mut self,
        mut parser: KittyParser,
        byte: u8,
        events: &mut Vec<Result<DecodedGraphics, GraphicsError>>,
    ) {
        if parser.ignored {
            if parser.escaped {
                parser.escaped = false;
                if byte == b'\\' {
                    self.reset();
                } else if byte == 0x18 || byte == 0x1a {
                    self.cancel_transfers();
                    self.reset();
                } else {
                    parser.escaped = byte == 0x1b;
                    self.state = GraphicsState::Kitty(parser);
                }
            } else if byte == 0x18 || byte == 0x1a {
                self.cancel_transfers();
                self.reset();
            } else if byte == 0x1b {
                parser.escaped = true;
                self.state = GraphicsState::Kitty(parser);
            } else if byte == 0x9c {
                self.reset();
            } else {
                self.state = GraphicsState::Kitty(parser);
            }
            return;
        }
        if parser.escaped {
            if byte == 0x18 || byte == 0x1a {
                self.cancel_transfers();
                self.reset();
            } else if byte == b'\\' {
                parser.escaped = false;
                self.finish_kitty(parser, events);
            } else {
                self.discard(
                    StringKind::Apc,
                    GraphicsError::InvalidCommand {
                        protocol: GraphicsProtocol::Kitty,
                    },
                    byte,
                );
            }
            return;
        }
        if byte == 0x18 || byte == 0x1a {
            self.cancel_transfers();
            self.reset();
        } else if byte == 0x1b {
            parser.escaped = true;
            self.state = GraphicsState::Kitty(parser);
        } else if byte == 0x9c {
            self.finish_kitty(parser, events);
        } else if let Err(error) = parser.feed(byte) {
            self.discard(StringKind::Apc, error, byte);
        } else {
            self.state = GraphicsState::Kitty(parser);
        }
    }

    fn feed_iterm(
        &mut self,
        mut parser: ItermParser,
        byte: u8,
        events: &mut Vec<Result<DecodedGraphics, GraphicsError>>,
    ) {
        if parser.escaped {
            if byte == 0x18 || byte == 0x1a {
                self.cancel_transfers();
                self.reset();
            } else if byte == b'\\' {
                parser.escaped = false;
                self.finish_iterm(parser, events);
            } else {
                self.discard(
                    StringKind::Osc,
                    GraphicsError::InvalidCommand {
                        protocol: GraphicsProtocol::Iterm2,
                    },
                    byte,
                );
            }
            return;
        }
        if byte == 0x18 || byte == 0x1a {
            self.cancel_transfers();
            self.reset();
        } else if byte == 0x1b {
            parser.escaped = true;
            self.state = GraphicsState::Iterm(parser);
        } else if byte == 0x07 || byte == 0x9c {
            self.finish_iterm(parser, events);
        } else if let Err(error) = parser.feed(byte) {
            self.discard(StringKind::Osc, error, byte);
        } else {
            self.state = GraphicsState::Iterm(parser);
        }
    }

    fn feed_tmux(
        &mut self,
        mut parser: TmuxParser,
        byte: u8,
        events: &mut Vec<Result<DecodedGraphics, GraphicsError>>,
    ) {
        if parser.prefix.len() < b"tmux;".len() {
            if byte == 0x9c {
                self.reset();
                return;
            }
            let expected = b"tmux;"[parser.prefix.len()];
            if byte != expected {
                self.ignore_string(StringKind::Dcs, byte);
            } else {
                parser.prefix.push(byte);
                self.state = GraphicsState::Tmux(parser);
            }
            return;
        }
        if byte == 0x9c {
            if !parser.inner_terminated
                && !self.body_has_complete_graphics(self.tmux_inner.as_deref(), &parser.data)
                && self.body_has_c1_terminated_graphics(self.tmux_inner.as_deref(), &parser.data)
            {
                if parser.data.len() == MAX_GRAPHICS_TRANSFER_BYTES {
                    self.finish_state(
                        Err(GraphicsError::TransferTooLarge {
                            protocol: GraphicsProtocol::Sixel,
                        }),
                        events,
                    );
                } else {
                    parser.data.push(byte);
                    parser.inner_terminated = true;
                    self.state = GraphicsState::Tmux(parser);
                }
            } else {
                self.finish_tmux(parser.data, events);
            }
            return;
        }
        if parser.escaped {
            parser.escaped = false;
            if byte == 0x18 || byte == 0x1a {
                self.cancel_transfers();
                self.reset();
            } else if byte == 0x1b {
                if parser.data.len() == MAX_GRAPHICS_TRANSFER_BYTES {
                    self.discard(
                        StringKind::Dcs,
                        GraphicsError::TransferTooLarge {
                            protocol: GraphicsProtocol::Sixel,
                        },
                        byte,
                    );
                } else {
                    parser.data.push(0x1b);
                    self.state = GraphicsState::Tmux(parser);
                }
            } else if byte == b'\\' {
                self.finish_tmux(parser.data, events);
            } else {
                self.discard(
                    StringKind::Dcs,
                    GraphicsError::InvalidCommand {
                        protocol: GraphicsProtocol::Sixel,
                    },
                    byte,
                );
            }
            return;
        }
        if byte == 0x18 || byte == 0x1a {
            self.cancel_transfers();
            self.reset();
        } else if byte == 0x1b {
            parser.escaped = true;
            self.state = GraphicsState::Tmux(parser);
        } else if parser.data.len() == MAX_GRAPHICS_TRANSFER_BYTES {
            self.discard(
                StringKind::Dcs,
                GraphicsError::TransferTooLarge {
                    protocol: GraphicsProtocol::Sixel,
                },
                byte,
            );
        } else {
            parser.data.push(byte);
            self.state = GraphicsState::Tmux(parser);
        }
    }

    fn feed_screen(
        &mut self,
        mut parser: ScreenParser,
        byte: u8,
        events: &mut Vec<Result<DecodedGraphics, GraphicsError>>,
    ) {
        if byte == 0x9c {
            let inner_complete = if parser.inner_terminated {
                self.body_has_c1_terminated_graphics_after_boundary(
                    self.screen_inner.as_deref(),
                    &parser,
                )
            } else {
                !self.body_has_complete_graphics(self.screen_inner.as_deref(), &parser.data)
                    && self
                        .body_has_c1_terminated_graphics(self.screen_inner.as_deref(), &parser.data)
            };
            if inner_complete {
                if parser.data.len() == MAX_SCREEN_PASSTHROUGH_BYTES {
                    self.finish_state(
                        Err(GraphicsError::TransferTooLarge {
                            protocol: GraphicsProtocol::Sixel,
                        }),
                        events,
                    );
                    self.screen_continuation = false;
                } else {
                    parser.data.push(byte);
                    parser.inner_terminated = true;
                    parser.inner_data_start = parser.data.len();
                    self.state = GraphicsState::Screen(parser);
                }
            } else {
                self.finish_screen(parser.data, events);
            }
            return;
        }
        if !parser.escaped && parser.data.as_slice() == [0x1b] && byte == b'\\' {
            self.finish_screen(Vec::new(), events);
            return;
        }
        if parser.escaped {
            parser.escaped = false;
            if byte == 0x18 || byte == 0x1a {
                self.cancel_transfers();
                self.reset();
            } else if byte == b'\\' {
                let inner_complete = if parser.inner_terminated {
                    self.body_has_st_terminated_graphics_after_boundary(
                        self.screen_inner.as_deref(),
                        &parser,
                    )
                } else {
                    !self.body_has_complete_graphics(self.screen_inner.as_deref(), &parser.data)
                        && self.body_has_st_terminated_graphics(
                            self.screen_inner.as_deref(),
                            &parser.data,
                        )
                };
                if inner_complete {
                    if parser.data.len() > MAX_SCREEN_PASSTHROUGH_BYTES.saturating_sub(2) {
                        self.finish_state(
                            Err(GraphicsError::TransferTooLarge {
                                protocol: GraphicsProtocol::Sixel,
                            }),
                            events,
                        );
                        self.screen_continuation = false;
                    } else {
                        parser.data.push(0x1b);
                        parser.data.push(b'\\');
                        parser.inner_terminated = true;
                        parser.inner_data_start = parser.data.len();
                        self.state = GraphicsState::Screen(parser);
                    }
                } else if parser.data.len() > MAX_SCREEN_PASSTHROUGH_BYTES {
                    self.finish_state(
                        Err(GraphicsError::TransferTooLarge {
                            protocol: GraphicsProtocol::Sixel,
                        }),
                        events,
                    );
                    self.screen_continuation = false;
                } else {
                    self.finish_screen(parser.data, events);
                }
            } else if parser.data.len() > MAX_SCREEN_PASSTHROUGH_BYTES.saturating_sub(2) {
                self.discard(
                    StringKind::Dcs,
                    GraphicsError::TransferTooLarge {
                        protocol: GraphicsProtocol::Sixel,
                    },
                    byte,
                );
            } else {
                parser.data.push(0x1b);
                parser.data.push(byte);
                self.state = GraphicsState::Screen(parser);
            }
            return;
        }
        if byte == 0x18 || byte == 0x1a {
            self.cancel_transfers();
            self.reset();
        } else if byte == 0x1b {
            parser.escaped = true;
            self.state = GraphicsState::Screen(parser);
        } else if parser.data.len() == MAX_SCREEN_PASSTHROUGH_BYTES {
            self.discard(
                StringKind::Dcs,
                GraphicsError::TransferTooLarge {
                    protocol: GraphicsProtocol::Sixel,
                },
                byte,
            );
        } else {
            parser.data.push(byte);
            self.state = GraphicsState::Screen(parser);
        }
    }

    fn finish_screen(
        &mut self,
        data: Vec<u8>,
        events: &mut Vec<Result<DecodedGraphics, GraphicsError>>,
    ) {
        let mut inner = self
            .screen_inner
            .take()
            .map(|inner| *inner)
            .unwrap_or_default();
        inner.wrapper_depth = self.wrapper_depth.saturating_add(1);
        inner.screen_continuation = false;
        for byte in data {
            inner.feed_byte(byte, events);
        }
        let continuation = inner.has_pending_state();
        self.reset();
        self.screen_continuation = continuation;
        if continuation {
            self.screen_inner = Some(Box::new(inner));
        }
    }

    fn finish_tmux(
        &mut self,
        data: Vec<u8>,
        events: &mut Vec<Result<DecodedGraphics, GraphicsError>>,
    ) {
        let mut inner = self
            .tmux_inner
            .take()
            .map(|inner| *inner)
            .unwrap_or_default();
        inner.wrapper_depth = self.wrapper_depth.saturating_add(1);
        inner.tmux_continuation = false;
        for byte in data {
            inner.feed_byte(byte, events);
        }
        let continuation = inner.has_pending_state();
        self.reset();
        self.tmux_continuation = continuation;
        if continuation {
            self.tmux_inner = Some(Box::new(inner));
        }
    }

    fn body_has_st_terminated_graphics(&self, inner: Option<&GraphicsParser>, data: &[u8]) -> bool {
        let mut candidate = data.to_vec();
        candidate.extend_from_slice(b"\x1b\\");
        matches!(self.decode_wrapper(inner, &candidate), Ok(Some(_)))
    }

    fn body_has_c1_terminated_graphics(&self, inner: Option<&GraphicsParser>, data: &[u8]) -> bool {
        let mut candidate = data.to_vec();
        candidate.push(0x9c);
        matches!(self.decode_wrapper(inner, &candidate), Ok(Some(_)))
    }

    fn body_has_st_terminated_graphics_after_boundary(
        &self,
        inner: Option<&GraphicsParser>,
        parser: &ScreenParser,
    ) -> bool {
        self.body_has_terminated_graphics_after_boundary(inner, parser, b"\x1b\\")
    }

    fn body_has_c1_terminated_graphics_after_boundary(
        &self,
        inner: Option<&GraphicsParser>,
        parser: &ScreenParser,
    ) -> bool {
        self.body_has_terminated_graphics_after_boundary(inner, parser, &[0x9c])
    }

    fn body_has_terminated_graphics_after_boundary(
        &self,
        inner: Option<&GraphicsParser>,
        parser: &ScreenParser,
        terminator: &[u8],
    ) -> bool {
        let mut replay = inner.cloned().unwrap_or_default();
        let _ = replay.advance(&parser.data[..parser.inner_data_start]);
        let mut candidate = parser.data[parser.inner_data_start..].to_vec();
        candidate.extend_from_slice(terminator);
        matches!(self.decode_wrapper(Some(&replay), &candidate), Ok(Some(_)))
    }

    fn body_has_complete_graphics(&self, inner: Option<&GraphicsParser>, data: &[u8]) -> bool {
        matches!(self.decode_wrapper(inner, data), Ok(Some(_)))
    }

    fn decode_wrapper(
        &self,
        inner: Option<&GraphicsParser>,
        bytes: &[u8],
    ) -> Result<Option<DecodedGraphics>, GraphicsError> {
        let mut parser = inner.cloned().unwrap_or_default();
        let events = parser.advance(bytes);
        match events.as_slice() {
            [event] => event.clone().map(Some),
            [] => Ok(None),
            _ => Err(GraphicsError::InvalidCommand {
                protocol: GraphicsProtocol::Sixel,
            }),
        }
    }

    fn feed_discard(
        &mut self,
        mut parser: DiscardParser,
        byte: u8,
        events: &mut Vec<Result<DecodedGraphics, GraphicsError>>,
    ) {
        if parser.escaped {
            parser.escaped = false;
            if byte == b'\\' {
                if parser.report {
                    self.finish_state(Err(parser.error), events);
                } else {
                    self.finish_state(Ok(None), events);
                }
            } else if byte == 0x18 || byte == 0x1a {
                self.cancel_transfers();
                self.reset();
            } else {
                parser.escaped = byte == 0x1b;
                self.state = GraphicsState::Discard(parser);
            }
            return;
        }
        if byte == 0x18 || byte == 0x1a {
            self.cancel_transfers();
            self.reset();
        } else if byte == 0x1b {
            parser.escaped = true;
            self.state = GraphicsState::Discard(parser);
        } else if byte == 0x9c || (parser.kind == StringKind::Osc && byte == 0x07) {
            if parser.report {
                self.finish_state(Err(parser.error), events);
            } else {
                self.finish_state(Ok(None), events);
            }
        } else {
            self.state = GraphicsState::Discard(parser);
        }
    }

    fn finish_kitty(
        &mut self,
        parser: KittyParser,
        events: &mut Vec<Result<DecodedGraphics, GraphicsError>>,
    ) {
        if parser.ignored {
            self.reset();
        } else {
            if self.abandoned_transfer == Some(GraphicsProtocol::Kitty) {
                match parser.finish() {
                    Ok(chunk) if chunk.more => self.reset(),
                    Ok(_) => {
                        self.abandoned_transfer = None;
                        self.finish_state(
                            Err(GraphicsError::TransferTooLarge {
                                protocol: GraphicsProtocol::Kitty,
                            }),
                            events,
                        );
                    }
                    Err(_) => self.reset(),
                }
            } else {
                let result = self.accept_kitty(parser.finish());
                self.finish_state(result, events);
            }
        }
    }

    fn finish_iterm(
        &mut self,
        parser: ItermParser,
        events: &mut Vec<Result<DecodedGraphics, GraphicsError>>,
    ) {
        if parser.ignored {
            self.reset();
        } else {
            match self.abandoned_transfer {
                Some(GraphicsProtocol::Iterm2) => match iterm_command_name(&parser.body) {
                    Some(b"FilePart") => self.reset(),
                    Some(b"FileEnd") => {
                        self.abandoned_transfer = None;
                        self.finish_state(
                            Err(GraphicsError::TransferTooLarge {
                                protocol: GraphicsProtocol::Iterm2,
                            }),
                            events,
                        );
                    }
                    _ => self.reset(),
                },
                _ => self.finish_iterm_command(parser, events),
            }
        }
    }

    fn finish_iterm_command(
        &mut self,
        parser: ItermParser,
        events: &mut Vec<Result<DecodedGraphics, GraphicsError>>,
    ) {
        let result = parse_iterm_command(&parser.body, &mut self.iterm_transfer);
        if self.iterm_transfer.is_some() {
            self.remember_transfer_sequence();
        }
        self.finish_state(result, events);
    }

    fn accept_kitty(
        &mut self,
        chunk: Result<KittyChunk, GraphicsError>,
    ) -> Result<Option<DecodedGraphics>, GraphicsError> {
        let chunk = chunk?;
        if let Some(mut transfer) = self.kitty_transfer.take() {
            validate_kitty_continuation(&transfer, &chunk)?;
            append_bounded(
                &mut transfer.encoded,
                &chunk.encoded,
                GraphicsProtocol::Kitty,
            )?;
            if chunk.more {
                self.kitty_transfer = Some(transfer);
                self.remember_transfer_sequence();
                Ok(None)
            } else {
                if let Some(expected) = chunk.declared_size {
                    if transfer.declared_size.is_some() && transfer.declared_size != Some(expected)
                    {
                        return Err(GraphicsError::InvalidHeader {
                            protocol: GraphicsProtocol::Kitty,
                        });
                    }
                    transfer.declared_size = Some(expected);
                }
                Ok(Some(finish_kitty_transfer(transfer)?))
            }
        } else if chunk.more {
            self.kitty_transfer = Some(KittyTransfer::from_chunk(chunk)?);
            self.remember_transfer_sequence();
            Ok(None)
        } else {
            Ok(Some(finish_kitty_chunk(chunk)?))
        }
    }

    fn finish_state(
        &mut self,
        result: Result<Option<DecodedGraphics>, GraphicsError>,
        events: &mut Vec<Result<DecodedGraphics, GraphicsError>>,
    ) {
        let failed = result.is_err();
        self.reset();
        match result {
            Ok(Some(image)) => events.push(Ok(image)),
            Ok(None) => {}
            Err(error) => events.push(Err(error)),
        }
        if failed || !self.has_own_transfer() {
            self.kitty_transfer = None;
            self.iterm_transfer = None;
            self.transfer_carry.clear();
            self.transfer_carryable = true;
        }
        if failed {
            self.screen_continuation = false;
            self.screen_inner = None;
            self.tmux_continuation = false;
            self.tmux_inner = None;
        }
    }

    fn discard(&mut self, kind: StringKind, error: GraphicsError, byte: u8) {
        self.discard_with_report(kind, error, byte, true);
    }

    fn discard_with_report(
        &mut self,
        kind: StringKind,
        error: GraphicsError,
        byte: u8,
        report: bool,
    ) {
        if report {
            if kind == StringKind::Apc {
                self.kitty_transfer = None;
            } else if kind == StringKind::Osc {
                self.iterm_transfer = None;
            }
            self.transfer_carry.clear();
            self.transfer_carryable = true;
        }
        self.state = GraphicsState::Discard(DiscardParser {
            kind,
            error,
            escaped: byte == 0x1b,
            report,
        });
    }

    fn ignore_string(&mut self, kind: StringKind, byte: u8) {
        self.discard_with_report(
            kind,
            GraphicsError::InvalidCommand {
                protocol: kind.protocol(),
            },
            byte,
            false,
        );
    }

    fn begin(&mut self, state: GraphicsState, byte: u8) {
        self.pending.clear();
        self.pending.push(byte);
        self.carryable = true;
        self.sequence_bytes = 1;
        self.state = state;
    }

    fn begin_transfer(&mut self, state: GraphicsState, byte: u8) {
        self.begin(state, byte);
        self.attach_transfer_carry();
    }

    fn begin_silent_string(&mut self, byte: u8) {
        self.begin(
            GraphicsState::Discard(DiscardParser {
                kind: StringKind::Dcs,
                error: GraphicsError::InvalidCommand {
                    protocol: GraphicsProtocol::Sixel,
                },
                escaped: false,
                report: false,
            }),
            byte,
        );
    }

    fn continue_transfer(&mut self, state: GraphicsState) {
        self.attach_transfer_carry();
        self.state = state;
    }

    fn attach_transfer_carry(&mut self) {
        if !self.has_own_transfer() {
            return;
        }
        if !self.transfer_carryable {
            self.carryable = false;
            return;
        }
        if self.transfer_carry.is_empty() {
            return;
        }
        let mut pending = std::mem::take(&mut self.transfer_carry);
        pending.extend_from_slice(&self.pending);
        if pending.len() > MAX_GRAPHICS_CARRY_BYTES {
            pending.clear();
            self.carryable = false;
        } else {
            self.pending = pending;
        }
    }

    fn push_pending(&mut self, byte: u8) {
        if !self.carryable {
            return;
        }
        if self.pending.len() == MAX_GRAPHICS_CARRY_BYTES {
            self.pending.clear();
            self.carryable = false;
        } else {
            self.pending.push(byte);
        }
    }

    fn reset(&mut self) {
        self.state = GraphicsState::Ground;
        self.pending.clear();
        self.carryable = true;
        self.sequence_bytes = 0;
    }

    fn cancel_transfers(&mut self) {
        self.kitty_transfer = None;
        self.iterm_transfer = None;
        self.transfer_carry.clear();
        self.transfer_carryable = true;
        self.abandoned_transfer = None;
        self.screen_continuation = false;
        self.screen_inner = None;
        self.tmux_continuation = false;
        self.tmux_inner = None;
    }

    fn remember_transfer_sequence(&mut self) {
        if !self.carryable {
            self.transfer_carry.clear();
            self.transfer_carryable = false;
            return;
        }
        if self.pending.len() > MAX_GRAPHICS_CARRY_BYTES {
            self.transfer_carry.clear();
            self.transfer_carryable = false;
            return;
        }
        self.transfer_carry.clear();
        self.transfer_carry.extend_from_slice(&self.pending);
    }
}

#[derive(Clone)]
struct SixelParser {
    phase: SixelPhase,
    header: Vec<u8>,
    command: Option<SixelCommand>,
    command_data: Vec<u8>,
    canvas: SixelCanvas,
    escaped: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SixelPhase {
    Header,
    Body,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SixelCommand {
    Repeat,
    Raster,
    Color,
}

impl SixelParser {
    fn new() -> Self {
        SixelParser {
            phase: SixelPhase::Header,
            header: Vec::new(),
            command: None,
            command_data: Vec::new(),
            canvas: SixelCanvas::new(),
            escaped: false,
        }
    }

    fn feed(&mut self, byte: u8) -> Result<(), GraphicsError> {
        if self.phase == SixelPhase::Header {
            if byte == b'q' {
                self.parse_header()?;
                self.phase = SixelPhase::Body;
            } else if byte.is_ascii_digit() || byte == b';' {
                push_bounded(
                    &mut self.header,
                    byte,
                    MAX_GRAPHICS_CONTROL_BYTES,
                    GraphicsProtocol::Sixel,
                )?;
            } else {
                return Err(GraphicsError::InvalidHeader {
                    protocol: GraphicsProtocol::Sixel,
                });
            }
            return Ok(());
        }

        if let Some(command) = self.command {
            if byte.is_ascii_digit() || byte == b';' {
                push_bounded(
                    &mut self.command_data,
                    byte,
                    MAX_GRAPHICS_CONTROL_BYTES,
                    GraphicsProtocol::Sixel,
                )?;
                return Ok(());
            }
            self.finish_command(command)?;
        }

        match byte {
            b'!' => {
                self.command = Some(SixelCommand::Repeat);
                self.command_data.clear();
            }
            b'"' => {
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
            _ => {
                return Err(GraphicsError::InvalidCommand {
                    protocol: GraphicsProtocol::Sixel,
                })
            }
        }
        Ok(())
    }

    fn finish(mut self) -> Result<Option<DecodedGraphics>, GraphicsError> {
        if let Some(command) = self.command.take() {
            self.finish_command(command)?;
            if command == SixelCommand::Repeat {
                return Err(GraphicsError::InvalidCommand {
                    protocol: GraphicsProtocol::Sixel,
                });
            }
        }
        if self.phase != SixelPhase::Body || !self.canvas.saw_sixel {
            return Err(GraphicsError::InvalidCommand {
                protocol: GraphicsProtocol::Sixel,
            });
        }
        Ok(Some(DecodedGraphics {
            protocol: GraphicsProtocol::Sixel,
            image: self.canvas.finish()?,
            action: ImageAction::Display,
            display: ImageDisplay {
                sixel_background: Some(self.canvas.background),
                ..ImageDisplay::default()
            },
        }))
    }

    fn parse_header(&mut self) -> Result<(), GraphicsError> {
        let params = parse_sixel_header_params(&self.header, 3)?;
        if let Some(aspect) = params.first().copied() {
            if aspect > 9 {
                return Err(GraphicsError::InvalidCommand {
                    protocol: GraphicsProtocol::Sixel,
                });
            }
        }
        match params.get(1).copied().unwrap_or(0) {
            0 | 2 => self.canvas.set_background(SixelBackground::Terminal),
            1 => self.canvas.set_background(SixelBackground::Preserve),
            _ => {
                return Err(GraphicsError::InvalidCommand {
                    protocol: GraphicsProtocol::Sixel,
                })
            }
        }
        Ok(())
    }

    fn finish_command(&mut self, command: SixelCommand) -> Result<(), GraphicsError> {
        let data = std::mem::take(&mut self.command_data);
        self.command = None;
        match command {
            SixelCommand::Repeat => {
                let count = parse_required_u32(&data, GraphicsProtocol::Sixel)?;
                if count == 0 {
                    return Err(GraphicsError::InvalidCommand {
                        protocol: GraphicsProtocol::Sixel,
                    });
                }
                self.canvas.repeat =
                    usize::try_from(count).map_err(|_| GraphicsError::ImageTooLarge {
                        protocol: GraphicsProtocol::Sixel,
                    })?;
            }
            SixelCommand::Raster => {
                let params = parse_sixel_params(&data, 4)?;
                if params.len() != 4 || params[2] == 0 || params[3] == 0 {
                    return Err(GraphicsError::InvalidDimensions {
                        protocol: GraphicsProtocol::Sixel,
                    });
                }
                self.canvas.set_raster(
                    usize::try_from(params[2]).map_err(|_| GraphicsError::ImageTooLarge {
                        protocol: GraphicsProtocol::Sixel,
                    })?,
                    usize::try_from(params[3]).map_err(|_| GraphicsError::ImageTooLarge {
                        protocol: GraphicsProtocol::Sixel,
                    })?,
                )?;
            }
            SixelCommand::Color => self.canvas.set_color(&data)?,
        }
        Ok(())
    }
}

#[derive(Clone)]
struct SixelCanvas {
    width: usize,
    height: usize,
    logical_width: usize,
    logical_height: usize,
    fixed_width: Option<usize>,
    fixed_height: Option<usize>,
    rgba: Vec<u8>,
    background: SixelBackground,
    palette: [[u8; 4]; 256],
    color: usize,
    x: usize,
    y: usize,
    repeat: usize,
    saw_sixel: bool,
}

impl SixelCanvas {
    fn new() -> Self {
        let mut palette = [[0, 0, 0, 255]; 256];
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
            palette[index] = [
                percentage_to_byte(red),
                percentage_to_byte(green),
                percentage_to_byte(blue),
                255,
            ];
        }
        SixelCanvas {
            width: 0,
            height: 0,
            logical_width: 0,
            logical_height: 0,
            fixed_width: None,
            fixed_height: None,
            rgba: Vec::new(),
            background: SixelBackground::Terminal,
            palette,
            color: 0,
            x: 0,
            y: 0,
            repeat: 1,
            saw_sixel: false,
        }
    }

    fn set_background(&mut self, background: SixelBackground) {
        self.background = background;
    }

    fn set_raster(&mut self, width: usize, height: usize) -> Result<(), GraphicsError> {
        if self.saw_sixel || self.fixed_width.is_some() {
            return Err(GraphicsError::InvalidCommand {
                protocol: GraphicsProtocol::Sixel,
            });
        }
        validate_dimensions(GraphicsProtocol::Sixel, width, height)?;
        self.fixed_width = Some(width);
        self.fixed_height = Some(height);
        self.width = width;
        self.height = height;
        self.logical_width = width;
        self.logical_height = height;
        self.rgba = filled_rgba(
            [0, 0, 0, 0],
            checked_rgba_len(GraphicsProtocol::Sixel, width, height)?,
        );
        Ok(())
    }

    fn set_color(&mut self, data: &[u8]) -> Result<(), GraphicsError> {
        let params = parse_sixel_params(data, 5)?;
        if params.is_empty() || params[0] > 255 {
            return Err(GraphicsError::InvalidCommand {
                protocol: GraphicsProtocol::Sixel,
            });
        }
        let index = usize::try_from(params[0]).map_err(|_| GraphicsError::InvalidCommand {
            protocol: GraphicsProtocol::Sixel,
        })?;
        if params.len() == 1 {
            self.color = index;
            return Ok(());
        }
        if params.len() != 5 {
            return Err(GraphicsError::InvalidCommand {
                protocol: GraphicsProtocol::Sixel,
            });
        }
        self.palette[index] = match params[1] {
            1 if params[2] <= 360 && params[3] <= 100 && params[4] <= 100 => {
                hls_to_rgba(params[2], params[3], params[4])
            }
            2 if params[2] <= 100 && params[3] <= 100 && params[4] <= 100 => [
                percentage_to_byte(params[2]),
                percentage_to_byte(params[3]),
                percentage_to_byte(params[4]),
                255,
            ],
            _ => {
                return Err(GraphicsError::InvalidCommand {
                    protocol: GraphicsProtocol::Sixel,
                })
            }
        };
        self.color = index;
        Ok(())
    }

    fn paint(&mut self, bits: u8) -> Result<(), GraphicsError> {
        let repeat = self.repeat;
        self.repeat = 1;
        let end_x = self
            .x
            .checked_add(repeat)
            .ok_or(GraphicsError::InvalidDimensions {
                protocol: GraphicsProtocol::Sixel,
            })?;
        if end_x > MAX_IMAGE_SIDE {
            return Err(GraphicsError::ImageTooLarge {
                protocol: GraphicsProtocol::Sixel,
            });
        }
        let end_y = self
            .y
            .checked_add(6)
            .ok_or(GraphicsError::InvalidDimensions {
                protocol: GraphicsProtocol::Sixel,
            })?;
        let needed_width = self.fixed_width.unwrap_or(end_x.max(self.logical_width));
        let needed_height = self.fixed_height.unwrap_or(end_y);
        if self.fixed_width.is_none() || self.fixed_height.is_none() {
            self.ensure_size(needed_width, needed_height)?;
        }
        self.saw_sixel = true;
        let draw_count = if let Some(fixed_width) = self.fixed_width {
            repeat.min(fixed_width.saturating_sub(self.x))
        } else {
            repeat
        };
        for offset in 0..draw_count {
            let x = self.x + offset;
            for bit in 0..6 {
                if bits & (1 << bit) == 0 {
                    continue;
                }
                let y = self.y + bit;
                if x < self.width && y < self.height {
                    let at = (y * self.width + x) * 4;
                    self.rgba[at..at + 4].copy_from_slice(&self.palette[self.color]);
                }
            }
        }
        self.logical_width = self.logical_width.max(end_x);
        self.logical_height = self.logical_height.max(end_y);
        self.x = end_x;
        Ok(())
    }

    fn carriage_return(&mut self) {
        self.x = 0;
    }

    fn new_line(&mut self) -> Result<(), GraphicsError> {
        self.x = 0;
        self.y = self
            .y
            .checked_add(6)
            .ok_or(GraphicsError::InvalidDimensions {
                protocol: GraphicsProtocol::Sixel,
            })?;
        Ok(())
    }

    fn ensure_size(&mut self, width: usize, height: usize) -> Result<(), GraphicsError> {
        validate_dimensions(GraphicsProtocol::Sixel, width, height)?;
        if width <= self.width && height <= self.height {
            return Ok(());
        }
        let grown_width = width.max(self.width.saturating_mul(2)).max(1);
        let grown_height = height.max(self.height.saturating_mul(2)).max(1);
        let (new_width, new_height) = if grown_width <= MAX_IMAGE_SIDE
            && grown_height <= MAX_IMAGE_SIDE
            && grown_width
                .checked_mul(grown_height)
                .is_some_and(|pixels| pixels <= MAX_IMAGE_PIXELS)
        {
            (grown_width, grown_height)
        } else {
            (width.max(self.width), height.max(self.height))
        };
        validate_dimensions(GraphicsProtocol::Sixel, new_width, new_height)?;
        let new_len = checked_rgba_len(GraphicsProtocol::Sixel, new_width, new_height)?;
        let mut rgba = filled_rgba([0, 0, 0, 0], new_len);
        for row in 0..self.height {
            let old_start = row * self.width * 4;
            let new_start = row * new_width * 4;
            rgba[new_start..new_start + self.width * 4]
                .copy_from_slice(&self.rgba[old_start..old_start + self.width * 4]);
        }
        self.width = new_width;
        self.height = new_height;
        self.rgba = rgba;
        Ok(())
    }

    fn finish(&self) -> Result<DecodedImage, GraphicsError> {
        let width = self.fixed_width.unwrap_or(self.logical_width);
        let height = self.fixed_height.unwrap_or(self.logical_height);
        validate_dimensions(GraphicsProtocol::Sixel, width, height)?;
        let len = checked_rgba_len(GraphicsProtocol::Sixel, width, height)?;
        let mut rgba = filled_rgba([0, 0, 0, 0], len);
        let columns = width.min(self.width);
        for row in 0..height.min(self.height) {
            let source_start = row * self.width * 4;
            let target_start = row * width * 4;
            rgba[target_start..target_start + columns * 4]
                .copy_from_slice(&self.rgba[source_start..source_start + columns * 4]);
        }
        Ok(DecodedImage {
            width: u32::try_from(width).map_err(|_| GraphicsError::ImageTooLarge {
                protocol: GraphicsProtocol::Sixel,
            })?,
            height: u32::try_from(height).map_err(|_| GraphicsError::ImageTooLarge {
                protocol: GraphicsProtocol::Sixel,
            })?,
            rgba,
        })
    }
}

#[derive(Clone)]
struct KittyParser {
    header: Vec<u8>,
    data: Vec<u8>,
    seen_header: bool,
    ignored: bool,
    escaped: bool,
}

impl KittyParser {
    fn new() -> Self {
        KittyParser {
            header: Vec::new(),
            data: Vec::new(),
            seen_header: false,
            ignored: false,
            escaped: false,
        }
    }

    fn feed(&mut self, byte: u8) -> Result<(), GraphicsError> {
        if self.ignored {
            return Ok(());
        }
        if !self.seen_header {
            if self.header.is_empty() && byte != b'G' {
                self.ignored = true;
                return Ok(());
            }
            if self.header.is_empty() {
                self.header.push(byte);
                return Ok(());
            }
            if byte == b';' {
                self.seen_header = true;
                return Ok(());
            }
            push_bounded(
                &mut self.header,
                byte,
                MAX_GRAPHICS_CONTROL_BYTES,
                GraphicsProtocol::Kitty,
            )?;
        } else {
            push_bounded(
                &mut self.data,
                byte,
                MAX_KITTY_CHUNK_BYTES,
                GraphicsProtocol::Kitty,
            )?;
        }
        Ok(())
    }

    fn finish(self) -> Result<KittyChunk, GraphicsError> {
        if !self.seen_header || self.header.first().copied() != Some(b'G') {
            return Err(GraphicsError::InvalidHeader {
                protocol: GraphicsProtocol::Kitty,
            });
        }
        let control = parse_kitty_control(&self.header[1..])?;
        KittyChunk::from_control(control, self.data)
    }
}

#[derive(Clone)]
struct ItermParser {
    prefix: Vec<u8>,
    body: Vec<u8>,
    prefix_done: bool,
    ignored: bool,
    escaped: bool,
}

impl ItermParser {
    fn new() -> Self {
        ItermParser {
            prefix: Vec::new(),
            body: Vec::new(),
            prefix_done: false,
            ignored: false,
            escaped: false,
        }
    }

    fn feed(&mut self, byte: u8) -> Result<(), GraphicsError> {
        if self.ignored {
            return Ok(());
        }
        if !self.prefix_done {
            if byte == b';' {
                if self.prefix.as_slice() != b"1337" {
                    self.ignored = true;
                } else {
                    self.prefix_done = true;
                }
                return Ok(());
            }
            if self.prefix.len() == 4
                || !byte.is_ascii_digit()
                || byte != b"1337"[self.prefix.len()]
            {
                self.ignored = true;
                return Ok(());
            }
            self.prefix.push(byte);
            return Ok(());
        }
        push_bounded(
            &mut self.body,
            byte,
            MAX_GRAPHICS_TRANSFER_BYTES,
            GraphicsProtocol::Iterm2,
        )
    }
}

#[derive(Clone)]
struct TmuxParser {
    prefix: Vec<u8>,
    data: Vec<u8>,
    escaped: bool,
    inner_terminated: bool,
}

impl TmuxParser {
    fn new() -> Self {
        Self::from_first(b't')
    }

    fn from_first(byte: u8) -> Self {
        TmuxParser {
            prefix: vec![byte],
            data: Vec::new(),
            escaped: false,
            inner_terminated: false,
        }
    }
}

#[derive(Clone)]
struct ScreenParser {
    data: Vec<u8>,
    escaped: bool,
    inner_terminated: bool,
    inner_data_start: usize,
}

impl ScreenParser {
    fn new() -> Self {
        Self::from_first(0x1b)
    }

    fn from_first(byte: u8) -> Self {
        ScreenParser {
            data: vec![byte],
            escaped: false,
            inner_terminated: false,
            inner_data_start: 0,
        }
    }
}

#[derive(Clone)]
struct KittyTransfer {
    id: Option<u32>,
    action: ImageAction,
    format: KittyFormat,
    width: Option<u32>,
    height: Option<u32>,
    display: ImageDisplay,
    encoded: Vec<u8>,
    compression: bool,
    declared_size: Option<usize>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum KittyFormat {
    Rgb,
    Rgba,
    Png,
}

#[derive(Clone)]
struct KittyChunk {
    id: Option<u32>,
    action: ImageAction,
    more: bool,
    more_specified: bool,
    continuation_compatible: bool,
    format: Option<KittyFormat>,
    width: Option<u32>,
    height: Option<u32>,
    display: ImageDisplay,
    encoded: Vec<u8>,
    compression: Option<bool>,
    declared_size: Option<usize>,
}

impl KittyChunk {
    fn from_control(control: KittyControl, data: Vec<u8>) -> Result<Self, GraphicsError> {
        if data.len() > MAX_KITTY_CHUNK_BYTES {
            return Err(GraphicsError::TransferTooLarge {
                protocol: GraphicsProtocol::Kitty,
            });
        }
        if control.more && (!data.len().is_multiple_of(4) || data.contains(&b'=')) {
            return Err(GraphicsError::InvalidBase64 {
                protocol: GraphicsProtocol::Kitty,
            });
        }
        if control.medium.unwrap_or(b'd') != b'd' {
            return Err(GraphicsError::UnsupportedAction {
                protocol: GraphicsProtocol::Kitty,
                action: format!(
                    "transfer medium {}",
                    control.medium.unwrap_or_default() as char
                ),
            });
        }
        if control.format == Some(KittyFormat::Png)
            && control.compression == Some(true)
            && control.declared_size.is_none()
        {
            return Err(GraphicsError::InvalidHeader {
                protocol: GraphicsProtocol::Kitty,
            });
        }
        if matches!(control.format, Some(KittyFormat::Rgb | KittyFormat::Rgba))
            && (control.width == Some(0) || control.height == Some(0))
        {
            return Err(GraphicsError::InvalidDimensions {
                protocol: GraphicsProtocol::Kitty,
            });
        }
        Ok(KittyChunk {
            id: control.id,
            action: control.action,
            more: control.more,
            more_specified: control.more_specified,
            continuation_compatible: control.continuation_compatible,
            format: control.format,
            width: control.width,
            height: control.height,
            display: control.display,
            encoded: data,
            compression: control.compression,
            declared_size: control.declared_size,
        })
    }
}

struct KittyControl {
    id: Option<u32>,
    action: ImageAction,
    medium: Option<u8>,
    format: Option<KittyFormat>,
    width: Option<u32>,
    height: Option<u32>,
    more: bool,
    more_specified: bool,
    compression: Option<bool>,
    declared_size: Option<usize>,
    display: ImageDisplay,
    continuation_compatible: bool,
}

impl Default for KittyControl {
    fn default() -> Self {
        KittyControl {
            id: None,
            action: ImageAction::Transmit,
            medium: None,
            format: None,
            width: None,
            height: None,
            more: false,
            more_specified: false,
            compression: None,
            declared_size: None,
            display: ImageDisplay::default(),
            continuation_compatible: true,
        }
    }
}

fn parse_kitty_control(data: &[u8]) -> Result<KittyControl, GraphicsError> {
    let mut control = KittyControl::default();
    let mut seen = [false; 256];
    for field in data.split(|byte| *byte == b',') {
        if field.is_empty() {
            continue;
        }
        let (key, value) = split_at_byte(field, b'=').ok_or(GraphicsError::InvalidHeader {
            protocol: GraphicsProtocol::Kitty,
        })?;
        if key.len() != 1 || value.is_empty() {
            return Err(GraphicsError::InvalidHeader {
                protocol: GraphicsProtocol::Kitty,
            });
        }
        let key = key[0];
        let slot = match key {
            b'a' | b'C' | b'c' | b'd' | b'f' | b'h' | b'H' | b'i' | b'I' | b'm' | b'N' | b'o'
            | b'O' | b'p' | b'P' | b'q' | b'Q' | b'r' | b's' | b'S' | b't' | b'U' | b'v' | b'V'
            | b'w' | b'x' | b'X' | b'y' | b'Y' | b'z' => usize::from(key),
            _ => {
                return Err(GraphicsError::InvalidHeader {
                    protocol: GraphicsProtocol::Kitty,
                })
            }
        };
        if seen[slot] {
            return Err(GraphicsError::InvalidHeader {
                protocol: GraphicsProtocol::Kitty,
            });
        }
        seen[slot] = true;
        if !matches!(key, b'm' | b'q') {
            control.continuation_compatible = false;
        }
        match key {
            b'a' => {
                let action = as_ascii(value, GraphicsProtocol::Kitty)?;
                control.action = match value {
                    b"t" => ImageAction::Transmit,
                    b"T" => ImageAction::TransmitAndDisplay,
                    _ => {
                        return Err(GraphicsError::UnsupportedAction {
                            protocol: GraphicsProtocol::Kitty,
                            action,
                        })
                    }
                };
            }
            b'f' => {
                control.format = Some(match parse_u32(value, GraphicsProtocol::Kitty)? {
                    24 => KittyFormat::Rgb,
                    32 => KittyFormat::Rgba,
                    100 => KittyFormat::Png,
                    _ => {
                        return Err(GraphicsError::UnsupportedMedia {
                            protocol: GraphicsProtocol::Kitty,
                            format: String::from_utf8_lossy(value).into_owned(),
                        })
                    }
                });
            }
            b'h' => {
                let height = parse_u32(value, GraphicsProtocol::Kitty)?;
                if height == 0 {
                    return Err(GraphicsError::InvalidDimensions {
                        protocol: GraphicsProtocol::Kitty,
                    });
                }
                control.display.height = Some(ImageDimension::Pixels(height));
            }
            b'i' => {
                if control.display.image_number.is_some() {
                    return Err(GraphicsError::InvalidHeader {
                        protocol: GraphicsProtocol::Kitty,
                    });
                }
                let id = parse_u32(value, GraphicsProtocol::Kitty)?;
                if id == 0 {
                    return Err(GraphicsError::InvalidCommand {
                        protocol: GraphicsProtocol::Kitty,
                    });
                }
                control.id = Some(id);
                control.display.image_id = Some(id);
            }
            b'I' => {
                if control.id.is_some() {
                    return Err(GraphicsError::InvalidHeader {
                        protocol: GraphicsProtocol::Kitty,
                    });
                }
                let id = parse_u32(value, GraphicsProtocol::Kitty)?;
                if id == 0 {
                    return Err(GraphicsError::InvalidCommand {
                        protocol: GraphicsProtocol::Kitty,
                    });
                }
                control.display.image_number = Some(id);
            }
            b'm' => match value {
                b"0" => {
                    control.more = false;
                    control.more_specified = true;
                }
                b"1" => {
                    control.more = true;
                    control.more_specified = true;
                }
                _ => {
                    return Err(GraphicsError::InvalidCommand {
                        protocol: GraphicsProtocol::Kitty,
                    })
                }
            },
            b'o' => {
                control.compression = Some(match value {
                    b"z" => true,
                    _ => {
                        return Err(GraphicsError::UnsupportedMedia {
                            protocol: GraphicsProtocol::Kitty,
                            format: as_ascii(value, GraphicsProtocol::Kitty)?,
                        })
                    }
                });
            }
            b's' => control.width = Some(parse_u32(value, GraphicsProtocol::Kitty)?),
            b'v' => control.height = Some(parse_u32(value, GraphicsProtocol::Kitty)?),
            b'p' => {
                control.display.placement_id = Some(parse_u32(value, GraphicsProtocol::Kitty)?);
            }
            b'S' => control.declared_size = Some(parse_usize(value, GraphicsProtocol::Kitty)?),
            b't' => {
                if value.len() != 1 {
                    return Err(GraphicsError::InvalidCommand {
                        protocol: GraphicsProtocol::Kitty,
                    });
                }
                control.medium = Some(value[0]);
            }
            b'c' => {
                control.display.cell_columns =
                    Some(parse_positive_u32(value, GraphicsProtocol::Kitty)?);
            }
            b'r' => {
                control.display.cell_rows =
                    Some(parse_positive_u32(value, GraphicsProtocol::Kitty)?);
            }
            b'w' => {
                control.display.width = Some(ImageDimension::Pixels(parse_positive_u32(
                    value,
                    GraphicsProtocol::Kitty,
                )?));
            }
            b'x' => {
                control.display.source_offset_x = Some(parse_u32(value, GraphicsProtocol::Kitty)?);
            }
            b'y' => {
                control.display.source_offset_y = Some(parse_u32(value, GraphicsProtocol::Kitty)?);
            }
            b'X' => {
                control.display.cell_offset_x = Some(parse_u32(value, GraphicsProtocol::Kitty)?);
            }
            b'Y' => {
                control.display.cell_offset_y = Some(parse_u32(value, GraphicsProtocol::Kitty)?);
            }
            b'C' => {
                control.display.move_cursor = match parse_u32(value, GraphicsProtocol::Kitty)? {
                    0 => true,
                    1 => false,
                    _ => {
                        return Err(GraphicsError::InvalidCommand {
                            protocol: GraphicsProtocol::Kitty,
                        })
                    }
                };
            }
            b'N' => {
                control.display.usage_hints = parse_u32(value, GraphicsProtocol::Kitty)?;
            }
            b'U' => {
                control.display.unicode_placeholder =
                    match parse_u32(value, GraphicsProtocol::Kitty)? {
                        0 => false,
                        1 => true,
                        _ => {
                            return Err(GraphicsError::InvalidCommand {
                                protocol: GraphicsProtocol::Kitty,
                            })
                        }
                    };
            }
            b'z' => {
                control.display.z_index = parse_i32(value, GraphicsProtocol::Kitty)?;
            }
            b'q' => {
                let quiet = parse_u32(value, GraphicsProtocol::Kitty)?;
                if quiet > 2 {
                    return Err(GraphicsError::InvalidCommand {
                        protocol: GraphicsProtocol::Kitty,
                    });
                }
            }
            b'd' | b'H' | b'O' | b'P' | b'Q' | b'V' => {
                return Err(GraphicsError::UnsupportedAction {
                    protocol: GraphicsProtocol::Kitty,
                    action: format!("control {}", key as char),
                });
            }
            _ => unreachable!(),
        }
    }
    Ok(control)
}

fn validate_kitty_continuation(
    transfer: &KittyTransfer,
    chunk: &KittyChunk,
) -> Result<(), GraphicsError> {
    if !chunk.more_specified || !chunk.continuation_compatible {
        return Err(GraphicsError::InvalidHeader {
            protocol: GraphicsProtocol::Kitty,
        });
    }
    if let Some(id) = chunk.id {
        if transfer.id != Some(id) {
            return Err(GraphicsError::InvalidHeader {
                protocol: GraphicsProtocol::Kitty,
            });
        }
    }
    if let Some(format) = chunk.format {
        if format != transfer.format {
            return Err(GraphicsError::InvalidHeader {
                protocol: GraphicsProtocol::Kitty,
            });
        }
    }
    if let Some(width) = chunk.width {
        if transfer.width != Some(width) {
            return Err(GraphicsError::InvalidDimensions {
                protocol: GraphicsProtocol::Kitty,
            });
        }
    }
    if let Some(height) = chunk.height {
        if transfer.height != Some(height) {
            return Err(GraphicsError::InvalidDimensions {
                protocol: GraphicsProtocol::Kitty,
            });
        }
    }
    if let Some(compression) = chunk.compression {
        if compression != transfer.compression {
            return Err(GraphicsError::InvalidCommand {
                protocol: GraphicsProtocol::Kitty,
            });
        }
    }
    if let Some(expected) = chunk.declared_size {
        if transfer.declared_size.is_some() && transfer.declared_size != Some(expected) {
            return Err(GraphicsError::InvalidHeader {
                protocol: GraphicsProtocol::Kitty,
            });
        }
    }
    Ok(())
}

impl KittyTransfer {
    fn from_chunk(chunk: KittyChunk) -> Result<Self, GraphicsError> {
        let format = chunk.format.unwrap_or(KittyFormat::Rgba);
        let width = chunk.width;
        let height = chunk.height;
        if matches!(format, KittyFormat::Rgb | KittyFormat::Rgba)
            && (width.is_none() || height.is_none())
        {
            return Err(GraphicsError::InvalidDimensions {
                protocol: GraphicsProtocol::Kitty,
            });
        }
        if let (Some(width), Some(height)) = (width, height) {
            validate_dimensions(
                GraphicsProtocol::Kitty,
                usize::try_from(width).map_err(|_| GraphicsError::ImageTooLarge {
                    protocol: GraphicsProtocol::Kitty,
                })?,
                usize::try_from(height).map_err(|_| GraphicsError::ImageTooLarge {
                    protocol: GraphicsProtocol::Kitty,
                })?,
            )?;
        }
        Ok(KittyTransfer {
            id: chunk.id,
            action: chunk.action,
            format,
            width,
            height,
            display: chunk.display,
            encoded: chunk.encoded,
            compression: chunk.compression.unwrap_or(false),
            declared_size: chunk.declared_size,
        })
    }
}

fn finish_kitty_chunk(chunk: KittyChunk) -> Result<DecodedGraphics, GraphicsError> {
    let transfer = KittyTransfer::from_chunk(chunk)?;
    finish_kitty_transfer(transfer)
}

fn finish_kitty_transfer(transfer: KittyTransfer) -> Result<DecodedGraphics, GraphicsError> {
    let bytes = decode_base64(GraphicsProtocol::Kitty, &transfer.encoded)?;
    let bytes = if transfer.compression {
        decompress_bounded(GraphicsProtocol::Kitty, &bytes)?
    } else {
        bytes
    };
    if let Some(expected) = transfer.declared_size {
        if expected != bytes.len() {
            return Err(GraphicsError::DeclaredSizeMismatch {
                protocol: GraphicsProtocol::Kitty,
                expected,
                actual: bytes.len(),
            });
        }
    }
    let image = match transfer.format {
        KittyFormat::Rgb => raw_rgb(
            GraphicsProtocol::Kitty,
            transfer.width.ok_or(GraphicsError::InvalidDimensions {
                protocol: GraphicsProtocol::Kitty,
            })?,
            transfer.height.ok_or(GraphicsError::InvalidDimensions {
                protocol: GraphicsProtocol::Kitty,
            })?,
            &bytes,
        )?,
        KittyFormat::Rgba => raw_rgba(
            GraphicsProtocol::Kitty,
            transfer.width.ok_or(GraphicsError::InvalidDimensions {
                protocol: GraphicsProtocol::Kitty,
            })?,
            transfer.height.ok_or(GraphicsError::InvalidDimensions {
                protocol: GraphicsProtocol::Kitty,
            })?,
            &bytes,
        )?,
        KittyFormat::Png => decode_raster(GraphicsProtocol::Kitty, &bytes)?,
    };
    Ok(DecodedGraphics {
        protocol: GraphicsProtocol::Kitty,
        image,
        action: transfer.action,
        display: transfer.display,
    })
}

fn iterm_command_name(body: &[u8]) -> Option<&[u8]> {
    (!body.is_empty()).then(|| split_at_byte(body, b'=').map_or(body, |(command, _)| command))
}

fn iterm_command_is_graphics(body: &[u8]) -> bool {
    match iterm_command_name(body) {
        None => true,
        Some(b"File" | b"MultipartFile" | b"FilePart" | b"FileEnd") => true,
        Some(_) => false,
    }
}

fn parse_iterm_command(
    body: &[u8],
    multipart: &mut Option<ItermTransfer>,
) -> Result<Option<DecodedGraphics>, GraphicsError> {
    if body.is_empty() {
        return Err(GraphicsError::InvalidHeader {
            protocol: GraphicsProtocol::Iterm2,
        });
    }
    let (command, rest) = split_at_byte(body, b'=').unwrap_or((body, &[]));
    match command {
        b"File" => {
            if multipart.is_some() {
                return Err(GraphicsError::MultipartState);
            }
            let (params, encoded) =
                split_at_byte(rest, b':').ok_or(GraphicsError::InvalidHeader {
                    protocol: GraphicsProtocol::Iterm2,
                })?;
            if params.len() > MAX_GRAPHICS_CONTROL_BYTES {
                return Err(GraphicsError::TransferTooLarge {
                    protocol: GraphicsProtocol::Iterm2,
                });
            }
            let meta = parse_iterm_meta(params, true)?;
            let bytes = decode_base64(GraphicsProtocol::Iterm2, encoded)?;
            check_declared_size(&meta, bytes.len())?;
            Ok(Some(DecodedGraphics {
                protocol: GraphicsProtocol::Iterm2,
                image: decode_raster(GraphicsProtocol::Iterm2, &bytes)?,
                action: ImageAction::Display,
                display: meta.display,
            }))
        }
        b"MultipartFile" => {
            if multipart.is_some() {
                return Err(GraphicsError::MultipartState);
            }
            let (params, encoded) = split_at_byte(rest, b':').unwrap_or((rest, &[]));
            if params.len() > MAX_GRAPHICS_CONTROL_BYTES {
                return Err(GraphicsError::TransferTooLarge {
                    protocol: GraphicsProtocol::Iterm2,
                });
            }
            let meta = parse_iterm_meta(params, true)?;
            if encoded.len() > MAX_GRAPHICS_TRANSFER_BYTES {
                return Err(GraphicsError::TransferTooLarge {
                    protocol: GraphicsProtocol::Iterm2,
                });
            }
            *multipart = Some(ItermTransfer {
                meta,
                encoded: encoded.to_vec(),
            });
            Ok(None)
        }
        b"FilePart" => {
            let transfer = multipart.as_mut().ok_or(GraphicsError::MultipartState)?;
            append_bounded(&mut transfer.encoded, rest, GraphicsProtocol::Iterm2)?;
            Ok(None)
        }
        b"FileEnd" => {
            if !rest.is_empty() {
                return Err(GraphicsError::InvalidHeader {
                    protocol: GraphicsProtocol::Iterm2,
                });
            }
            let transfer = multipart.take().ok_or(GraphicsError::MultipartState)?;
            let bytes = decode_base64(GraphicsProtocol::Iterm2, &transfer.encoded)?;
            check_declared_size(&transfer.meta, bytes.len())?;
            Ok(Some(DecodedGraphics {
                protocol: GraphicsProtocol::Iterm2,
                image: decode_raster(GraphicsProtocol::Iterm2, &bytes)?,
                action: ImageAction::Display,
                display: transfer.meta.display,
            }))
        }
        _ => Ok(None),
    }
}

#[derive(Clone)]
struct ItermMeta {
    display: ImageDisplay,
    declared_size: Option<usize>,
}

#[derive(Clone)]
struct ItermTransfer {
    meta: ItermMeta,
    encoded: Vec<u8>,
}

fn parse_iterm_meta(data: &[u8], require_inline: bool) -> Result<ItermMeta, GraphicsError> {
    if data.len() > MAX_GRAPHICS_CONTROL_BYTES {
        return Err(GraphicsError::TransferTooLarge {
            protocol: GraphicsProtocol::Iterm2,
        });
    }
    let mut display = ImageDisplay::default();
    let mut declared_size = None;
    let mut inline = false;
    let mut seen: Vec<&[u8]> = Vec::new();
    for field in data.split(|byte| *byte == b';') {
        if field.is_empty() {
            continue;
        }
        let (key, value) = split_at_byte(field, b'=').ok_or(GraphicsError::InvalidHeader {
            protocol: GraphicsProtocol::Iterm2,
        })?;
        if seen.contains(&key) {
            return Err(GraphicsError::InvalidHeader {
                protocol: GraphicsProtocol::Iterm2,
            });
        }
        seen.push(key);
        match key {
            b"inline" => match value {
                b"1" => inline = true,
                b"0" => inline = false,
                _ => {
                    return Err(GraphicsError::InvalidCommand {
                        protocol: GraphicsProtocol::Iterm2,
                    })
                }
            },
            b"size" => declared_size = Some(parse_usize(value, GraphicsProtocol::Iterm2)?),
            b"width" => display.width = Some(parse_iterm_dimension(value)?),
            b"height" => display.height = Some(parse_iterm_dimension(value)?),
            b"preserveAspectRatio" => match value {
                b"1" => display.preserve_aspect_ratio = true,
                b"0" => display.preserve_aspect_ratio = false,
                _ => {
                    return Err(GraphicsError::InvalidCommand {
                        protocol: GraphicsProtocol::Iterm2,
                    })
                }
            },
            b"name" => {
                if value.len() > MAX_GRAPHICS_CONTROL_BYTES {
                    return Err(GraphicsError::TransferTooLarge {
                        protocol: GraphicsProtocol::Iterm2,
                    });
                }
            }
            _ => {
                if value.len() > MAX_GRAPHICS_CONTROL_BYTES {
                    return Err(GraphicsError::TransferTooLarge {
                        protocol: GraphicsProtocol::Iterm2,
                    });
                }
            }
        }
    }
    if require_inline && !inline {
        return Err(GraphicsError::UnsupportedAction {
            protocol: GraphicsProtocol::Iterm2,
            action: "inline=0".to_string(),
        });
    }
    if let Some(size) = declared_size {
        if size > MAX_GRAPHICS_TRANSFER_BYTES {
            return Err(GraphicsError::TransferTooLarge {
                protocol: GraphicsProtocol::Iterm2,
            });
        }
    }
    Ok(ItermMeta {
        display,
        declared_size,
    })
}

fn parse_iterm_dimension(value: &[u8]) -> Result<ImageDimension, GraphicsError> {
    if value == b"auto" {
        return Ok(ImageDimension::Auto);
    }
    if value.ends_with(b"px") {
        let number = parse_u32(
            &value[..value.len().saturating_sub(2)],
            GraphicsProtocol::Iterm2,
        )?;
        if number == 0 {
            return Err(GraphicsError::InvalidDimensions {
                protocol: GraphicsProtocol::Iterm2,
            });
        }
        return Ok(ImageDimension::Pixels(number));
    }
    if value.ends_with(b"%") {
        let number = parse_u32(
            &value[..value.len().saturating_sub(1)],
            GraphicsProtocol::Iterm2,
        )?;
        if number == 0 || number > 100 {
            return Err(GraphicsError::InvalidDimensions {
                protocol: GraphicsProtocol::Iterm2,
            });
        }
        return Ok(ImageDimension::Percent(u16::try_from(number).map_err(
            |_| GraphicsError::InvalidDimensions {
                protocol: GraphicsProtocol::Iterm2,
            },
        )?));
    }
    let number = parse_u32(value, GraphicsProtocol::Iterm2)?;
    if number == 0 {
        return Err(GraphicsError::InvalidDimensions {
            protocol: GraphicsProtocol::Iterm2,
        });
    }
    Ok(ImageDimension::Cells(number))
}

fn check_declared_size(meta: &ItermMeta, actual: usize) -> Result<(), GraphicsError> {
    if let Some(expected) = meta.declared_size {
        if expected != actual {
            return Err(GraphicsError::DeclaredSizeMismatch {
                protocol: GraphicsProtocol::Iterm2,
                expected,
                actual,
            });
        }
    }
    Ok(())
}

fn decode_base64(protocol: GraphicsProtocol, data: &[u8]) -> Result<Vec<u8>, GraphicsError> {
    if data.len() > MAX_GRAPHICS_TRANSFER_BYTES {
        return Err(GraphicsError::TransferTooLarge { protocol });
    }
    let decoded = STANDARD
        .decode(data)
        .or_else(|_| STANDARD_NO_PAD.decode(data))
        .map_err(|_| GraphicsError::InvalidBase64 { protocol })?;
    if decoded.len() > MAX_GRAPHICS_TRANSFER_BYTES {
        return Err(GraphicsError::TransferTooLarge { protocol });
    }
    Ok(decoded)
}

fn decompress_bounded(protocol: GraphicsProtocol, data: &[u8]) -> Result<Vec<u8>, GraphicsError> {
    let mut decoder = ZlibDecoder::new(Cursor::new(data));
    let mut output = Vec::new();
    decoder
        .by_ref()
        .take((MAX_IMAGE_BYTES + 1) as u64)
        .read_to_end(&mut output)
        .map_err(|_| GraphicsError::DecodeFailure { protocol })?;
    if output.len() > MAX_IMAGE_BYTES {
        return Err(GraphicsError::ImageTooLarge { protocol });
    }
    if decoder.into_inner().position() != data.len() as u64 {
        return Err(GraphicsError::DecodeFailure { protocol });
    }
    Ok(output)
}

fn decode_raster(protocol: GraphicsProtocol, data: &[u8]) -> Result<DecodedImage, GraphicsError> {
    if data.len() > MAX_IMAGE_BYTES {
        return Err(GraphicsError::ImageTooLarge { protocol });
    }
    let format = image::guess_format(data).map_err(|_| GraphicsError::UnsupportedMedia {
        protocol,
        format: "unknown".to_string(),
    })?;
    let format_name = format!("{format:?}");
    if !matches!(
        format,
        image::ImageFormat::Bmp
            | image::ImageFormat::Gif
            | image::ImageFormat::Jpeg
            | image::ImageFormat::Png
            | image::ImageFormat::Tiff
            | image::ImageFormat::WebP
    ) {
        return Err(GraphicsError::UnsupportedMedia {
            protocol,
            format: format_name,
        });
    }
    if let Some(format) = animated_raster_format(protocol, format, data)? {
        return Err(GraphicsError::UnsupportedMedia {
            protocol,
            format: format.to_string(),
        });
    }
    let decoded = catch_unwind(AssertUnwindSafe(|| {
        let mut reader = image::ImageReader::new(Cursor::new(data))
            .with_guessed_format()
            .map_err(|_| GraphicsError::DecodeFailure { protocol })?;
        reader.limits(raster_limits());
        reader
            .decode()
            .map(|image| image.into_rgba8())
            .map_err(|error| map_image_error(protocol, error))
    }))
    .map_err(|_| GraphicsError::DecodeFailure { protocol })??;
    let (width, height) = decoded.dimensions();
    let width = usize::try_from(width).map_err(|_| GraphicsError::ImageTooLarge { protocol })?;
    let height = usize::try_from(height).map_err(|_| GraphicsError::ImageTooLarge { protocol })?;
    validate_dimensions(protocol, width, height)?;
    let rgba = decoded.into_raw();
    let expected = checked_rgba_len(protocol, width, height)?;
    if rgba.len() != expected {
        return Err(GraphicsError::DecodeFailure { protocol });
    }
    Ok(DecodedImage {
        width: u32::try_from(width).map_err(|_| GraphicsError::ImageTooLarge { protocol })?,
        height: u32::try_from(height).map_err(|_| GraphicsError::ImageTooLarge { protocol })?,
        rgba,
    })
}

fn animated_raster_format(
    protocol: GraphicsProtocol,
    format: image::ImageFormat,
    data: &[u8],
) -> Result<Option<&'static str>, GraphicsError> {
    match format {
        image::ImageFormat::Gif => gif_has_multiple_frames(protocol, data)
            .map(|animated| animated.then_some("animated GIF")),
        image::ImageFormat::Png => {
            png_is_animated(protocol, data).map(|animated| animated.then_some("animated PNG"))
        }
        image::ImageFormat::WebP => {
            webp_is_animated(protocol, data).map(|animated| animated.then_some("animated WebP"))
        }
        _ => Ok(None),
    }
}

fn png_is_animated(protocol: GraphicsProtocol, data: &[u8]) -> Result<bool, GraphicsError> {
    catch_unwind(AssertUnwindSafe(|| {
        let decoder =
            image::codecs::png::PngDecoder::with_limits(Cursor::new(data), raster_limits())
                .map_err(|error| map_image_error(protocol, error))?;
        decoder
            .is_apng()
            .map_err(|error| map_image_error(protocol, error))
    }))
    .map_err(|_| GraphicsError::DecodeFailure { protocol })?
}

fn webp_is_animated(protocol: GraphicsProtocol, data: &[u8]) -> Result<bool, GraphicsError> {
    use image::ImageDecoder;

    catch_unwind(AssertUnwindSafe(|| {
        let mut decoder = image::codecs::webp::WebPDecoder::new(Cursor::new(data))
            .map_err(|error| map_image_error(protocol, error))?;
        decoder
            .set_limits(raster_limits())
            .map_err(|error| map_image_error(protocol, error))?;
        Ok(decoder.has_animation())
    }))
    .map_err(|_| GraphicsError::DecodeFailure { protocol })?
}

fn gif_has_multiple_frames(protocol: GraphicsProtocol, data: &[u8]) -> Result<bool, GraphicsError> {
    use image::{AnimationDecoder, ImageDecoder};

    catch_unwind(AssertUnwindSafe(|| {
        let mut decoder = image::codecs::gif::GifDecoder::new(Cursor::new(data))
            .map_err(|error| map_image_error(protocol, error))?;
        decoder
            .set_limits(raster_limits())
            .map_err(|error| map_image_error(protocol, error))?;
        let mut frames = decoder.into_frames();
        frames
            .next()
            .transpose()
            .map_err(|error| map_image_error(protocol, error))?
            .ok_or(GraphicsError::DecodeFailure { protocol })?;
        match frames.next() {
            None => Ok(false),
            Some(Ok(_)) => Ok(true),
            Some(Err(error)) => Err(map_image_error(protocol, error)),
        }
    }))
    .map_err(|_| GraphicsError::DecodeFailure { protocol })?
}

fn map_image_error(protocol: GraphicsProtocol, error: image::ImageError) -> GraphicsError {
    match error {
        image::ImageError::Limits(limit)
            if matches!(
                limit.kind(),
                image::error::LimitErrorKind::DimensionError
                    | image::error::LimitErrorKind::InsufficientMemory
            ) =>
        {
            GraphicsError::ImageTooLarge { protocol }
        }
        _ => GraphicsError::DecodeFailure { protocol },
    }
}

fn raster_limits() -> image::Limits {
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_IMAGE_SIDE as u32);
    limits.max_image_height = Some(MAX_IMAGE_SIDE as u32);
    limits.max_alloc = Some(MAX_IMAGE_BYTES as u64);
    limits
}

fn raw_rgb(
    protocol: GraphicsProtocol,
    width: u32,
    height: u32,
    data: &[u8],
) -> Result<DecodedImage, GraphicsError> {
    let width = usize::try_from(width).map_err(|_| GraphicsError::ImageTooLarge { protocol })?;
    let height = usize::try_from(height).map_err(|_| GraphicsError::ImageTooLarge { protocol })?;
    validate_dimensions(protocol, width, height)?;
    let expected = width
        .checked_mul(height)
        .and_then(|pixels| pixels.checked_mul(3))
        .ok_or(GraphicsError::InvalidDimensions { protocol })?;
    if data.len() != expected {
        return Err(GraphicsError::DeclaredSizeMismatch {
            protocol,
            expected,
            actual: data.len(),
        });
    }
    let mut rgba = Vec::with_capacity(checked_rgba_len(protocol, width, height)?);
    for pixel in data.chunks_exact(3) {
        rgba.extend_from_slice(pixel);
        rgba.push(255);
    }
    Ok(DecodedImage {
        width: u32::try_from(width).map_err(|_| GraphicsError::ImageTooLarge { protocol })?,
        height: u32::try_from(height).map_err(|_| GraphicsError::ImageTooLarge { protocol })?,
        rgba,
    })
}

fn raw_rgba(
    protocol: GraphicsProtocol,
    width: u32,
    height: u32,
    data: &[u8],
) -> Result<DecodedImage, GraphicsError> {
    let width = usize::try_from(width).map_err(|_| GraphicsError::ImageTooLarge { protocol })?;
    let height = usize::try_from(height).map_err(|_| GraphicsError::ImageTooLarge { protocol })?;
    validate_dimensions(protocol, width, height)?;
    let expected = checked_rgba_len(protocol, width, height)?;
    if data.len() != expected {
        return Err(GraphicsError::DeclaredSizeMismatch {
            protocol,
            expected,
            actual: data.len(),
        });
    }
    Ok(DecodedImage {
        width: u32::try_from(width).map_err(|_| GraphicsError::ImageTooLarge { protocol })?,
        height: u32::try_from(height).map_err(|_| GraphicsError::ImageTooLarge { protocol })?,
        rgba: data.to_vec(),
    })
}

fn validate_dimensions(
    protocol: GraphicsProtocol,
    width: usize,
    height: usize,
) -> Result<(), GraphicsError> {
    if width == 0 || height == 0 || width > MAX_IMAGE_SIDE || height > MAX_IMAGE_SIDE {
        return Err(GraphicsError::ImageTooLarge { protocol });
    }
    let pixels = width
        .checked_mul(height)
        .ok_or(GraphicsError::InvalidDimensions { protocol })?;
    if pixels > MAX_IMAGE_PIXELS {
        return Err(GraphicsError::ImageTooLarge { protocol });
    }
    Ok(())
}

fn checked_rgba_len(
    protocol: GraphicsProtocol,
    width: usize,
    height: usize,
) -> Result<usize, GraphicsError> {
    validate_dimensions(protocol, width, height)?;
    width
        .checked_mul(height)
        .and_then(|pixels| pixels.checked_mul(4))
        .filter(|length| *length <= MAX_IMAGE_BYTES)
        .ok_or(GraphicsError::InvalidDimensions { protocol })
}

fn parse_sixel_params(data: &[u8], max: usize) -> Result<Vec<u32>, GraphicsError> {
    if data.is_empty() {
        return Ok(Vec::new());
    }
    let mut result = Vec::new();
    for part in data.split(|byte| *byte == b';') {
        if result.len() == max || part.is_empty() {
            return Err(GraphicsError::InvalidCommand {
                protocol: GraphicsProtocol::Sixel,
            });
        }
        result.push(parse_u32(part, GraphicsProtocol::Sixel)?);
    }
    Ok(result)
}

fn parse_sixel_header_params(data: &[u8], max: usize) -> Result<Vec<u32>, GraphicsError> {
    if data.is_empty() {
        return Ok(Vec::new());
    }
    let mut result = Vec::new();
    for part in data.split(|byte| *byte == b';') {
        if result.len() == max {
            return Err(GraphicsError::InvalidHeader {
                protocol: GraphicsProtocol::Sixel,
            });
        }
        result.push(if part.is_empty() {
            0
        } else {
            parse_u32(part, GraphicsProtocol::Sixel)?
        });
    }
    Ok(result)
}

fn parse_required_u32(data: &[u8], protocol: GraphicsProtocol) -> Result<u32, GraphicsError> {
    if data.is_empty() {
        return Err(GraphicsError::InvalidCommand { protocol });
    }
    parse_u32(data, protocol)
}

fn parse_u32(data: &[u8], protocol: GraphicsProtocol) -> Result<u32, GraphicsError> {
    if data.is_empty() || !data.iter().all(u8::is_ascii_digit) {
        return Err(GraphicsError::InvalidCommand { protocol });
    }
    let mut value = 0u32;
    for &byte in data {
        value = value
            .checked_mul(10)
            .and_then(|value| value.checked_add(u32::from(byte - b'0')))
            .ok_or(GraphicsError::InvalidDimensions { protocol })?;
    }
    Ok(value)
}

fn parse_i32(data: &[u8], protocol: GraphicsProtocol) -> Result<i32, GraphicsError> {
    if data.is_empty() {
        return Err(GraphicsError::InvalidCommand { protocol });
    }
    let (negative, digits) = if data[0] == b'-' {
        (true, &data[1..])
    } else {
        (false, data)
    };
    if digits.is_empty() || !digits.iter().all(u8::is_ascii_digit) {
        return Err(GraphicsError::InvalidCommand { protocol });
    }
    let mut magnitude = 0u32;
    for &byte in digits {
        magnitude = magnitude
            .checked_mul(10)
            .and_then(|value| value.checked_add(u32::from(byte - b'0')))
            .ok_or(GraphicsError::InvalidCommand { protocol })?;
    }
    if negative {
        if magnitude == 2_147_483_648 {
            Ok(i32::MIN)
        } else {
            i32::try_from(magnitude)
                .ok()
                .and_then(|value| value.checked_neg())
                .ok_or(GraphicsError::InvalidCommand { protocol })
        }
    } else {
        i32::try_from(magnitude).map_err(|_| GraphicsError::InvalidCommand { protocol })
    }
}

fn parse_positive_u32(data: &[u8], protocol: GraphicsProtocol) -> Result<u32, GraphicsError> {
    let value = parse_u32(data, protocol)?;
    if value == 0 {
        return Err(GraphicsError::InvalidDimensions { protocol });
    }
    Ok(value)
}

fn parse_usize(data: &[u8], protocol: GraphicsProtocol) -> Result<usize, GraphicsError> {
    let value = parse_u32(data, protocol)?;
    usize::try_from(value).map_err(|_| GraphicsError::InvalidDimensions { protocol })
}

fn as_ascii(data: &[u8], protocol: GraphicsProtocol) -> Result<String, GraphicsError> {
    if !data.is_ascii() {
        return Err(GraphicsError::InvalidHeader { protocol });
    }
    Ok(String::from_utf8_lossy(data).into_owned())
}

fn append_bounded(
    target: &mut Vec<u8>,
    bytes: &[u8],
    protocol: GraphicsProtocol,
) -> Result<(), GraphicsError> {
    let new_len = target
        .len()
        .checked_add(bytes.len())
        .ok_or(GraphicsError::InvalidDimensions { protocol })?;
    if new_len > MAX_GRAPHICS_TRANSFER_BYTES {
        return Err(GraphicsError::TransferTooLarge { protocol });
    }
    target.extend_from_slice(bytes);
    Ok(())
}

fn push_bounded(
    target: &mut Vec<u8>,
    byte: u8,
    limit: usize,
    protocol: GraphicsProtocol,
) -> Result<(), GraphicsError> {
    if target.len() == limit {
        return Err(GraphicsError::TransferTooLarge { protocol });
    }
    target.push(byte);
    Ok(())
}

fn split_at_byte(data: &[u8], delimiter: u8) -> Option<(&[u8], &[u8])> {
    let index = data.iter().position(|byte| *byte == delimiter)?;
    Some((&data[..index], &data[index + 1..]))
}

fn filled_rgba(color: [u8; 4], length: usize) -> Vec<u8> {
    debug_assert_eq!(length % 4, 0);
    let mut rgba = vec![0; length];
    for pixel in rgba.chunks_exact_mut(4) {
        pixel.copy_from_slice(&color);
    }
    rgba
}

fn hls_to_rgba(hue: u32, lightness: u32, saturation: u32) -> [u8; 4] {
    let lightness = f64::from(lightness) / 100.0;
    if saturation == 0 {
        let channel = float_to_byte(lightness);
        return [channel, channel, channel, 255];
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
        255,
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

#[cfg(test)]
mod tests;
