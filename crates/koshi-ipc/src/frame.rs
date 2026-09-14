//! One painted frame: the picture the session composed for one attached
//! client, at that client's own viewport and scroll position, ready to draw.
//!
//! The session solves the client's active tab, cuts each pane's visible window
//! out of its grid, and resolves that client's highlight, then sends the result
//! as a [`PaintedFrame`](crate::frame::PaintedFrame). The client draws what
//! arrives. Every attached client gets its own frame: a client on an 80×24
//! terminal and one on a 200×50 terminal receive different frames of the same
//! session in the same instant.
//!
//! Scrollback rows never travel here. A pane sends the rows its window shows
//! this frame and nothing else, plus the two numbers the scroll indicator is
//! drawn from — [`is_truncated`](crate::frame::FrameScrollback::is_truncated) and
//! [`retained_line_count`](crate::frame::FrameScrollback::retained_line_count). A client
//! scrolled 500 lines back over a 24-row pane receives those 24 rows, never the
//! 500 above them.
//!
//! Rows are run-length encoded.
//! [`FrameRow::from_cells`](crate::frame::FrameRow::from_cells) folds each
//! stretch of equal neighbouring cells into one
//! [`FrameRun`](crate::frame::FrameRun): a blank 80-column row travels as a
//! single run with `count == 80`, and
//! [`FrameRow::expand_cells`](crate::frame::FrameRow::expand_cells) expands the runs back
//! into the same 80 cells.
//!
//! A field this build does not know is ignored, in this record and every one
//! under it: a frame from a newer koshi still draws. The four value enums —
//! [`FrameCursorShape`](crate::frame::FrameCursorShape),
//! [`FrameRowEnd`](crate::frame::FrameRowEnd),
//! [`FrameColor`](crate::frame::FrameColor) and
//! [`FrameUnderline`](crate::frame::FrameUnderline) — fall back to their
//! plainest value when this build has no name for what arrives. Image
//! presentation fields use the same rule: an unknown image protocol reads as
//! Kitty, an unknown image action as `Display`, and an unknown optional image
//! dimension or Sixel background reads as absent. A cell whose underline
//! arrives as `"Dotted2"` draws with no underline; every other cell in the
//! frame is unaffected.

use koshi_core::geometry::{Rect, Size};
use koshi_core::ids::{ClientId, PaneId, SessionId, TabId};
use koshi_core::lock::LockMode;
use koshi_core::mouse::MouseTracking;
use koshi_layout::mode::LayoutMode;
use koshi_layout::solver::StackHeader;
use koshi_pane::pane::state::PaneKind;
use serde::de::Error as _;
use serde::{Deserialize, Serialize};

const MAX_FRAME_IMAGE_SIDE_PIXEL_COUNT: u32 = 16_384;
const MAX_FRAME_IMAGE_PIXEL_COUNT: u64 = 16_777_216;

/// The maximum total RGBA bytes a chunked painted frame may carry.
pub const MAX_FRAME_IMAGE_TRANSFER_BYTE_COUNT: u64 = 67_108_864;

/// The largest raw image chunk carried by one session event.
pub const MAX_FRAME_IMAGE_CHUNK_BYTE_COUNT: usize = 1_048_576;

/// The largest number of image transfers accepted after one painted frame.
pub const MAX_FRAME_IMAGE_TRANSFER_COUNT: usize = 4_096;

/// Decode an image presentation value through an owned JSON value so the
/// fallback works for transport input and for owned deserialization callers.
fn deserialize_image_or_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::de::DeserializeOwned + Default,
{
    let image_value = serde_json::Value::deserialize(deserializer)?;
    Ok(T::deserialize(image_value).unwrap_or_default())
}

/// One frame, as it travels to a client: the session's active tab, the content
/// of every pane in it, and the viewing client's own state.
///
/// A reader joins [`pane_snapshots`](Self::pane_snapshots) to the
/// [`FrameSlot`]s in [`session_snapshot`](Self::session_snapshot)'s active tab
/// by [`PaneId`]: a slot says *where* a pane sits, its [`FramePane`] says
/// *what* is inside it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaintedFrame {
    /// The session being viewed: its identity, its solved active tab, and its
    /// tab list.
    #[serde(rename = "session")]
    pub session_snapshot: FrameSession,
    /// Per-pane content, one entry per live pane in the active tab, matched to
    /// a [`FrameSlot`] by [`PaneId`].
    #[serde(rename = "panes")]
    pub pane_snapshots: Vec<FramePane>,
    /// The viewing client's own state (viewport, focus, lock mode).
    #[serde(rename = "client")]
    pub client_snapshot: FrameClient,
}

/// The session-scoped part of a frame: identity, the solved active tab, and the
/// entries the tab bar is drawn from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrameSession {
    /// The session's stable id.
    #[serde(rename = "id")]
    pub session_id: SessionId,
    /// The session's display name.
    #[serde(rename = "name")]
    pub session_name: String,
    /// The tab this client is shown, solved and ready to draw.
    #[serde(rename = "active_tab")]
    pub active_tab_snapshot: FrameTab,
    /// One entry per tab in the session, in display order.
    #[serde(rename = "tabs")]
    pub tab_snapshots: Vec<FrameTabMeta>,
}

/// The active tab, with its layout already solved into placed pane slots.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrameTab {
    /// The tab's stable id.
    #[serde(rename = "id")]
    pub tab_id: TabId,
    /// The tab's display name.
    #[serde(rename = "name")]
    pub tab_name: String,
    /// The solved layout: one [`FrameSlot`] per pane, giving outer and content
    /// rects and coarse status.
    #[serde(rename = "slots")]
    pub pane_slots: Vec<FrameSlot>,
    /// The viewport size the layout was solved for: the element-wise minimum
    /// viewport across the clients viewing this tab. The
    /// [`pane_slots`](Self::pane_slots) rects live in this space with origin `(0, 0)`. A
    /// client whose own [`viewport_size`](FrameClient::viewport_size) is larger draws
    /// this layout centered and letterboxes the surrounding margin.
    #[serde(rename = "effective_size")]
    pub effective_cell_size: Size,
    /// Header strips for stacked panes: the one-row title bar each collapsed
    /// stack member shows in place of its content.
    pub stack_headers: Vec<StackHeader>,
    /// Whether this client sees the tab tiled, or sees a single pane zoomed to
    /// fill it. Zoom is per client: another client viewing the same tab in
    /// the same instant can carry a different value here.
    pub layout_mode: LayoutMode,
    /// True when the tab has no room to draw and every pane is suppressed;
    /// the client fills the whole frame with the "terminal too small" overlay.
    #[serde(rename = "all_suppressed")]
    pub is_every_pane_suppressed: bool,
    /// Blank cells between two panes that meet along a horizontal or
    /// vertical split, in the [`pane_slots`](Self::pane_slots) space. A frame from a
    /// server without this field reads as `0`, and so does a value that is
    /// not a cell count.
    #[serde(default, deserialize_with = "crate::wire::deserialize_or_default")]
    #[serde(rename = "gap")]
    pub gap_cell_count: u16,
}

/// One tab's entry in the tab bar: enough to draw the tab list without its
/// layout.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrameTabMeta {
    /// The tab's stable id.
    #[serde(rename = "id")]
    pub tab_id: TabId,
    /// The tab's display name.
    #[serde(rename = "name")]
    pub tab_name: String,
    /// The tab's position in the bar, starting at 0.
    #[serde(rename = "index")]
    pub tab_index: usize,
    /// Whether this is the client's active tab, drawn with the active marker.
    #[serde(rename = "active")]
    pub is_active: bool,
}

/// One pane's placement in the solved layout: where its box sits, its content
/// area, and coarse status flags. Paired with a [`FramePane`] by
/// [`pane_id`](Self::pane_id).
///
/// [`is_visible`](Self::is_visible) is true exactly when
/// [`content_rect`](Self::content_rect) is `Some`, and an
/// [`is_suppressed`](Self::is_suppressed) pane is not visible. [`is_dead`](Self::is_dead)
/// is a separate axis: an exited pane stays laid out, drawn dimmed, until it is
/// removed. `content_rect` is `None` for three distinct reasons — no room,
/// hidden, or a collapsed stack member — and [`is_suppressed`](Self::is_suppressed)
/// marks the no-room case.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrameSlot {
    /// The pane this slot places.
    pub pane_id: PaneId,
    /// The outer pane box, including the 1-cell border gutter.
    #[serde(rename = "rect")]
    pub outer_rect: Rect,
    /// The content area inside the border — the rect the PTY was sized from.
    /// `None` when the pane shows no content (suppressed, hidden, or a
    /// collapsed stack member). Cells and the cursor are drawn here.
    #[serde(rename = "inner_rect")]
    pub content_rect: Option<Rect>,
    /// Whether a terminal or a plugin backs this pane.
    #[serde(rename = "kind")]
    pub pane_kind: PaneKind,
    /// Whether the pane is currently shown.
    #[serde(rename = "visible")]
    pub is_visible: bool,
    /// Whether the pane is suppressed for lack of room.
    #[serde(rename = "suppressed")]
    pub is_suppressed: bool,
    /// Whether the pane's process has exited.
    #[serde(rename = "dead")]
    pub is_dead: bool,
}

/// One image placement carried with a pane's visible cells.
///
/// `content_id` names an image record uploaded on this connection. A client
/// without that record still has the complete cell rectangle needed to draw
/// the unavailable-image marker.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FrameImagePlacement {
    /// The complete cell size and the clipped top and left cells.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(rename = "geometry")]
    pub cell_geometry: Option<koshi_core::geometry::ImageCellGeometry>,
    /// Record metadata for this placement of the shared pixel content.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(rename = "record")]
    pub image_record: Option<FrameImageRecordHeader>,
    /// The terminal-local placement identity.
    #[serde(rename = "id")]
    pub placement_id: u64,
    /// The connection-local image-record identity.
    #[serde(rename = "content_id")]
    pub image_content_id: u64,
    /// Whether the source snapshot has the named image record.
    #[serde(rename = "available")]
    pub is_available: bool,
    /// The zero-based row and column of the upper-left covered cell.
    #[serde(rename = "anchor")]
    pub anchor_cell: (u16, u16),
    /// The number of covered columns.
    #[serde(rename = "columns")]
    pub column_count: u16,
    /// The number of covered rows.
    #[serde(rename = "rows")]
    pub row_count: u16,
}

impl<'de> Deserialize<'de> for FrameImagePlacement {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct FrameImagePlacementFields {
            #[serde(rename = "id")]
            placement_id: u64,
            #[serde(rename = "content_id")]
            image_content_id: u64,
            #[serde(default)]
            #[serde(rename = "geometry")]
            cell_geometry: Option<koshi_core::geometry::ImageCellGeometry>,
            #[serde(default)]
            #[serde(rename = "record")]
            image_record: Option<FrameImageRecordHeader>,
            #[serde(default = "is_image_available_by_default")]
            #[serde(rename = "available")]
            is_available: bool,
            #[serde(rename = "anchor")]
            anchor_cell: (u16, u16),
            #[serde(rename = "columns")]
            column_count: u16,
            #[serde(rename = "rows")]
            row_count: u16,
        }

        let placement_fields = FrameImagePlacementFields::deserialize(deserializer)?;
        if placement_fields.cell_geometry.is_some_and(|cell_geometry| {
            !cell_geometry.is_visible_size_contained(koshi_core::geometry::Size {
                column_count: placement_fields.column_count,
                row_count: placement_fields.row_count,
            })
        }) {
            return Err(D::Error::custom(
                "image clipping exceeds its complete cell dimensions",
            ));
        }
        if placement_fields.placement_id == 0 {
            return Err(D::Error::custom(
                "image placement identity must not be zero",
            ));
        }
        if placement_fields.image_content_id == 0 {
            return Err(D::Error::custom("image content identity must not be zero"));
        }
        if placement_fields.column_count == 0 || placement_fields.row_count == 0 {
            return Err(D::Error::custom(
                "image placement dimensions must not be zero",
            ));
        }
        let coordinate_end = u32::from(u16::MAX) + 1;
        if u32::from(placement_fields.anchor_cell.0) + u32::from(placement_fields.row_count)
            > coordinate_end
            || u32::from(placement_fields.anchor_cell.1) + u32::from(placement_fields.column_count)
                > coordinate_end
        {
            return Err(D::Error::custom(
                "image placement exceeds the cell coordinate range",
            ));
        }
        Ok(Self {
            placement_id: placement_fields.placement_id,
            image_content_id: placement_fields.image_content_id,
            cell_geometry: placement_fields.cell_geometry,
            image_record: placement_fields.image_record,
            is_available: placement_fields.is_available,
            anchor_cell: placement_fields.anchor_cell,
            column_count: placement_fields.column_count,
            row_count: placement_fields.row_count,
        })
    }
}

const fn is_image_available_by_default() -> bool {
    true
}

/// Image metadata sent before its RGBA bytes arrive in chunks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrameImageRecordHeader {
    /// Protocol that supplied the image. An unknown protocol reads as Kitty.
    #[serde(default, deserialize_with = "deserialize_image_or_default")]
    pub protocol: FrameGraphicsProtocol,
    /// Image width in pixels.
    #[serde(rename = "width")]
    pub pixel_width: u32,
    /// Image height in pixels.
    #[serde(rename = "height")]
    pub pixel_height: u32,
    /// The state operation represented by the transfer. An unknown action reads
    /// as `Display`.
    #[serde(default, deserialize_with = "deserialize_image_or_default")]
    #[serde(rename = "action")]
    pub image_action: FrameImageAction,
    /// Display hints supplied by the protocol.
    pub display: FrameImageDisplay,
    /// Cursor position when the image sequence ended, as row and column.
    #[serde(rename = "anchor")]
    pub anchor_cell: (u16, u16),
}

/// One image record whose RGBA bytes travel in separate bounded events.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FrameImageTransfer {
    /// The image-record identity on this connection.
    #[serde(rename = "id")]
    pub image_content_id: u64,
    /// The image record metadata and its pixel dimensions.
    #[serde(rename = "record")]
    pub image_record: FrameImageRecordHeader,
    /// The exact number of RGBA bytes the chunks carry.
    #[serde(rename = "byte_len")]
    pub image_byte_count: u64,
}

impl<'de> Deserialize<'de> for FrameImageTransfer {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct FrameImageTransferFields {
            #[serde(rename = "id")]
            image_content_id: u64,
            #[serde(rename = "record")]
            image_record: FrameImageRecordHeader,
            #[serde(rename = "byte_len")]
            image_byte_count: u64,
        }

        let transfer_fields = FrameImageTransferFields::deserialize(deserializer)?;
        if transfer_fields.image_content_id == 0 {
            return Err(D::Error::custom("image transfer identity must not be zero"));
        }
        if transfer_fields.image_record.image_action == FrameImageAction::Transmit {
            return Err(D::Error::custom(
                "a transmitted-only image cannot be an image placement",
            ));
        }
        let expected_byte_count = compute_frame_image_byte_count(
            transfer_fields.image_record.pixel_width,
            transfer_fields.image_record.pixel_height,
        )
        .map_err(D::Error::custom)?;
        if transfer_fields.image_byte_count != expected_byte_count {
            return Err(D::Error::custom(
                "image transfer byte length does not match its dimensions",
            ));
        }
        Ok(Self {
            image_content_id: transfer_fields.image_content_id,
            image_record: transfer_fields.image_record,
            image_byte_count: transfer_fields.image_byte_count,
        })
    }
}

/// One bounded part of one chunked image transfer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FrameImageChunk {
    /// The image-record identity named by the transfer start.
    #[serde(rename = "transfer_id")]
    pub image_transfer_id: u64,
    /// The raw-byte offset of `bytes` in that image.
    #[serde(rename = "offset")]
    pub byte_offset: u64,
    /// Whether this chunk ends the transfer.
    #[serde(rename = "last")]
    pub is_last: bool,
    /// Raw RGBA bytes, encoded as base64 on the wire.
    #[serde(with = "crate::bytes::base64_or_list")]
    #[serde(rename = "bytes")]
    pub chunk_bytes: Vec<u8>,
}

impl<'de> Deserialize<'de> for FrameImageChunk {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct FrameImageChunkFields {
            #[serde(rename = "transfer_id")]
            image_transfer_id: u64,
            #[serde(rename = "offset")]
            byte_offset: u64,
            #[serde(rename = "last")]
            is_last: bool,
            #[serde(with = "crate::bytes::base64_or_list")]
            #[serde(rename = "bytes")]
            chunk_bytes: Vec<u8>,
        }

        let chunk_fields = FrameImageChunkFields::deserialize(deserializer)?;
        if chunk_fields.image_transfer_id == 0 {
            return Err(D::Error::custom("image transfer identity must not be zero"));
        }
        if chunk_fields.chunk_bytes.is_empty() {
            return Err(D::Error::custom("image chunk must not be empty"));
        }
        if chunk_fields.chunk_bytes.len() > MAX_FRAME_IMAGE_CHUNK_BYTE_COUNT {
            return Err(D::Error::custom("image chunk exceeds its byte limit"));
        }
        Ok(Self {
            image_transfer_id: chunk_fields.image_transfer_id,
            byte_offset: chunk_fields.byte_offset,
            is_last: chunk_fields.is_last,
            chunk_bytes: chunk_fields.chunk_bytes,
        })
    }
}

/// A graphics protocol named in a frame.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum FrameGraphicsProtocol {
    /// DEC Sixel raster data.
    Sixel,
    /// Kitty graphics data.
    #[default]
    Kitty,
    /// iTerm2 inline image data.
    Iterm2,
}

/// A requested image dimension from a frame display field.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum FrameImageDimension {
    /// A number of terminal cells.
    Cells(u32),
    /// A number of device pixels.
    Pixels(u32),
    /// A percentage of the available terminal area.
    Percent(u16),
    /// Let the terminal choose the dimension.
    #[default]
    Auto,
}

/// The Sixel zero-bit background rule carried by a frame image.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum FrameSixelBackground {
    /// Use the terminal background for zero bits.
    Terminal,
    /// Keep zero bits transparent.
    #[default]
    Preserve,
}

/// Display hints carried by a frame image.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct FrameImageDisplay {
    /// The source command's response suppression level.
    #[serde(
        rename = "quiet",
        skip_serializing_if = "is_zero_response_suppression_level"
    )]
    pub response_suppression_level: u8,
    /// The requested width, if the sender supplied one. An unknown dimension is
    /// read as absent.
    #[serde(deserialize_with = "deserialize_image_or_default")]
    #[serde(rename = "width")]
    pub requested_width: Option<FrameImageDimension>,
    /// The requested height, if the sender supplied one. An unknown dimension is
    /// read as absent.
    #[serde(deserialize_with = "deserialize_image_or_default")]
    #[serde(rename = "height")]
    pub requested_height: Option<FrameImageDimension>,
    /// Whether the sender requests aspect-ratio preservation.
    #[serde(rename = "preserve_aspect_ratio")]
    pub is_aspect_ratio_preserved: bool,
    /// The Sixel background rule, when the record came from Sixel. An unknown
    /// rule is read as absent.
    #[serde(deserialize_with = "deserialize_image_or_default")]
    pub sixel_background: Option<FrameSixelBackground>,
    /// The Kitty image id, when one was supplied.
    pub image_id: Option<u32>,
    /// The Kitty image number, when one was supplied.
    pub image_number: Option<u32>,
    /// The Kitty placement id, when one was supplied.
    pub placement_id: Option<u32>,
    /// Usage flags supplied by Kitty.
    pub usage_hints: u32,
    /// Whether Kitty asks for a Unicode-placeholder placement.
    #[serde(rename = "unicode_placeholder")]
    pub is_unicode_placeholder: bool,
    /// The Kitty image z-index.
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
    /// The number of terminal columns requested by Kitty.
    #[serde(rename = "cell_columns")]
    pub requested_column_count: Option<u32>,
    /// The number of terminal rows requested by Kitty.
    #[serde(rename = "cell_rows")]
    pub requested_row_count: Option<u32>,
    /// The source image x offset requested by Kitty, in pixels.
    #[serde(rename = "source_offset_x")]
    pub source_pixel_offset_x: Option<u32>,
    /// The source image y offset requested by Kitty, in pixels.
    #[serde(rename = "source_offset_y")]
    pub source_pixel_offset_y: Option<u32>,
    /// The x offset inside the first terminal cell requested by Kitty.
    #[serde(rename = "cell_offset_x")]
    pub cell_pixel_offset_x: Option<u32>,
    /// The y offset inside the first terminal cell requested by Kitty.
    #[serde(rename = "cell_offset_y")]
    pub cell_pixel_offset_y: Option<u32>,
    /// Whether Kitty asks the placement to move the cursor after display.
    #[serde(rename = "move_cursor")]
    pub should_move_cursor: bool,
}

impl Default for FrameImageDisplay {
    fn default() -> Self {
        Self {
            response_suppression_level: 0,
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
        }
    }
}

fn is_zero_response_suppression_level(response_suppression_level: &u8) -> bool {
    *response_suppression_level == 0
}

/// The transfer action recorded with a frame image.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum FrameImageAction {
    /// Transmit the decoded image without requesting display.
    Transmit,
    /// Place a decoded image without a Kitty image transfer.
    #[default]
    Display,
    /// Transmit and place the decoded image in one operation.
    TransmitAndDisplay,
}

/// Validate frame image dimensions and return the exact RGBA byte count.
fn compute_frame_image_byte_count(
    pixel_width: u32,
    pixel_height: u32,
) -> Result<u64, &'static str> {
    let pixel_count = u64::from(pixel_width)
        .checked_mul(u64::from(pixel_height))
        .ok_or("image dimensions overflow")?;
    let image_byte_count = pixel_count
        .checked_mul(4)
        .ok_or("image byte count overflows")?;
    if pixel_width == 0
        || pixel_height == 0
        || pixel_width > MAX_FRAME_IMAGE_SIDE_PIXEL_COUNT
        || pixel_height > MAX_FRAME_IMAGE_SIDE_PIXEL_COUNT
        || pixel_count > MAX_FRAME_IMAGE_PIXEL_COUNT
        || image_byte_count > MAX_FRAME_IMAGE_TRANSFER_BYTE_COUNT
    {
        return Err("image dimensions exceed graphics limits");
    }
    Ok(image_byte_count)
}

/// The viewing client's own state: what this client sees and how it is moded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrameClient {
    /// The client's stable id.
    #[serde(rename = "id")]
    pub client_id: ClientId,
    /// The client's terminal size in cells.
    #[serde(rename = "viewport")]
    pub viewport_size: Size,
    /// The tab the client is viewing.
    #[serde(rename = "active_tab")]
    pub active_tab_id: TabId,
    /// The client's focused pane in the active tab, or `None` when the tab has
    /// no focusable pane. The client highlights the pane whose
    /// [`FrameSlot::pane_id`] matches, and places the cursor there.
    #[serde(rename = "focused_pane")]
    pub focused_pane_id: Option<PaneId>,
    /// The client's input mode, as the session has it.
    pub lock_mode: LockMode,
    /// Whether this client grabs the mouse for text selection. Adds the
    /// `SELECT` tag to the mode indicator, and decides whether a press in a
    /// mouse-aware pane begins a highlight.
    #[serde(rename = "mouse_select")]
    pub is_mouse_selection_enabled: bool,
}

/// One pane's content: the cells drawn inside the matching [`FrameSlot`]'s
/// content rect, plus what a mouse event over this pane is answered from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FramePane {
    /// The pane this content belongs to, matched to a [`FrameSlot`] by id.
    #[serde(rename = "id")]
    pub pane_id: PaneId,
    /// The pane's resolved display title: on the alternate screen the running
    /// app's OSC 0/1/2 title; on the primary screen the shell's OSC 7 working
    /// directory (`~`-shortened), falling back to the OSC title. `None` when
    /// the pane has reported neither.
    #[serde(rename = "title")]
    pub pane_title: Option<String>,
    /// The cursor's position and look within the content area.
    #[serde(rename = "cursor")]
    pub cursor_snapshot: FrameCursor,
    /// The visible terminal cells. `None` for a pane with no terminal content —
    /// a plugin pane, or a slot showing nothing this frame.
    #[serde(rename = "window")]
    pub terminal_window: Option<FrameWindow>,
    /// The complete image placements whose rectangles fit inside this pane's
    /// visible window. Empty when the pane has no image to draw.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[serde(rename = "image_placements")]
    pub image_placement_snapshots: Vec<FrameImagePlacement>,
    /// Whether the whole screen is in reverse video (DECSCNM): the client swaps
    /// the default foreground and background for every cell.
    #[serde(rename = "reverse_video")]
    pub is_reverse_video: bool,
    /// Which mouse events the pane's program asked to be told about
    /// (`?9`/`?1000`/`?1002`/`?1003`). Present in every frame: a pane that
    /// asked for nothing sends [`MouseTracking::Off`].
    pub mouse_tracking: MouseTracking,
    /// Whether alternate-scroll mode (`?1007`) is on: on the alternate screen a
    /// wheel tick becomes cursor arrow keys.
    #[serde(rename = "alt_scroll")]
    pub is_alt_scroll_enabled: bool,
    /// Whether the pane is showing the alternate screen. The alternate screen
    /// keeps no scrollback and has no view to scroll.
    #[serde(rename = "on_alt_screen")]
    pub is_on_alt_screen: bool,
    /// The absolute line number of the top row this frame shows for the pane,
    /// counting every line the pane has ever pushed into scrollback. A press on
    /// the pane's `n`-th visible row names line `view_top_row_index + n`.
    #[serde(rename = "view_top_row")]
    pub view_top_row_index: u64,
    /// The viewing client's highlighted text in this pane, cut down to the rows
    /// this frame shows. `None` when the client has nothing highlighted here,
    /// or when the highlight is entirely outside the visible rows.
    #[serde(rename = "selection")]
    pub selection_spans: Option<FrameSelection>,
    /// Whether the viewing client has a highlight in this pane at all,
    /// including one scrolled entirely out of the visible rows, where
    /// [`selection_spans`](Self::selection_spans) is `None`.
    pub has_selection: bool,
    /// Scrollback state for the scroll-position indicator.
    #[serde(rename = "scrollback")]
    pub scrollback_meta: FrameScrollback,
}

/// The cursor's position within the content area, and how it is drawn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrameCursor {
    /// The cursor's row within the content area, starting at 0.
    #[serde(rename = "row")]
    pub row_index: u16,
    /// The cursor's column within the content area, starting at 0.
    #[serde(rename = "col")]
    pub column_index: u16,
    /// Whether the cursor is visible.
    #[serde(rename = "visible")]
    pub is_visible: bool,
    /// Whether the cursor blinks.
    #[serde(rename = "blink")]
    pub is_blinking: bool,
    /// The shape the cursor is drawn as (DECSCUSR), or `None` while the pane
    /// has asked for no shape at all, which leaves the user's own configured
    /// cursor standing. A shape this build has no name for reads as `None`.
    #[serde(default, deserialize_with = "crate::wire::deserialize_or_default")]
    pub shape: Option<FrameCursorShape>,
}

/// A cursor shape a pane asked for with DECSCUSR. Mirrors
/// `koshi_terminal::state::CursorShape`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum FrameCursorShape {
    /// A box filling the whole cell.
    Block,
    /// A line along the bottom of the cell.
    Underline,
    /// A vertical bar at the cell's left edge.
    Bar,
}

/// Which cells of a pane are highlighted this frame, as a column range per
/// visible row.
///
/// Rows are in ascending order, and a row the highlight does not touch has no
/// entry. A highlight running from mid-way along row 4 to mid-way along row 6
/// of an 80-column pane arrives as `[(4, 12, 79), (5, 0, 79), (6, 0, 33)]`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrameSelection {
    /// One entry per highlighted row: the row, then the first and last
    /// highlighted column on it. Both columns are inclusive.
    #[serde(rename = "rows")]
    pub row_spans: Vec<(u16, u16, u16)>,
}

/// Scrollback state the scroll-position indicator is drawn from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrameScrollback {
    /// Whether the buffer reached its cap and dropped its oldest lines.
    #[serde(rename = "truncated")]
    pub is_truncated: bool,
    /// How many scrollback lines are currently retained.
    #[serde(rename = "retained_lines")]
    pub retained_line_count: usize,
}

/// The cells a pane shows this frame, run-length encoded row by row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrameWindow {
    /// The width every row expands back to.
    #[serde(rename = "cols")]
    pub column_count: u16,
    /// The visible rows, top row first.
    #[serde(rename = "rows")]
    pub row_snapshots: Vec<FrameRow>,
    /// Rows scrolled up from the live tail; `0` shows the live bottom of the
    /// buffer.
    #[serde(rename = "view_offset")]
    pub view_row_offset: usize,
}

/// One row of cells, as runs of equal neighbouring cells.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrameRow {
    /// The runs, left to right. Their counts sum to the row's width.
    #[serde(rename = "runs")]
    pub cell_runs: Vec<FrameRun>,
    /// Whether the row ends its logical line or continues onto the next. An
    /// ending this build has no name for reads as
    /// [`Hard`](FrameRowEnd::Hard).
    #[serde(
        default,
        deserialize_with = "crate::wire::deserialize_or_default",
        skip_serializing_if = "FrameRowEnd::is_hard"
    )]
    #[serde(rename = "end")]
    pub row_end: FrameRowEnd,
}

/// How a row ends: the wire form of the terminal's per-row line-continuation
/// state.
///
/// [`Hard`](Self::Hard) is left off the wire and read back as the default;
/// the two wrapped endings travel with their row.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum FrameRowEnd {
    /// The row ends its logical line: the next row starts a new one.
    #[default]
    Hard,
    /// The row soft-wrapped under autowrap: the next row continues this row's
    /// logical line.
    Soft,
    /// The row soft-wrapped when a wide glyph did not fit its last column;
    /// that last cell is a blank spacer.
    SoftWide,
}

impl FrameRowEnd {
    /// Whether this is the default [`Hard`](FrameRowEnd::Hard), which is left
    /// off the wire.
    #[must_use]
    pub fn is_hard(&self) -> bool {
        matches!(self, FrameRowEnd::Hard)
    }
}

impl FrameRow {
    /// Fold `frame_cells` into runs: each stretch of equal neighbouring cells becomes
    /// one [`FrameRun`]. A count stops at [`u16::MAX`] and the next equal cell
    /// opens a new run. `end` is how the row ends its logical line.
    ///
    /// 80 blank cells give one run with `count == 80`. 70 000 blank cells give
    /// two runs, `65_535` then `4_465`.
    #[must_use]
    pub fn from_cells(
        frame_cells: impl IntoIterator<Item = FrameCell>,
        row_end: FrameRowEnd,
    ) -> Self {
        let mut cell_runs: Vec<FrameRun> = Vec::new();
        for frame_cell in frame_cells {
            match cell_runs.last_mut() {
                Some(cell_run)
                    if cell_run.cell == frame_cell && cell_run.repeat_count < u16::MAX =>
                {
                    cell_run.repeat_count += 1;
                }
                _ => cell_runs.push(FrameRun {
                    repeat_count: 1,
                    cell: frame_cell,
                }),
            }
        }
        Self { cell_runs, row_end }
    }

    /// Expand the runs back into cells, each run's cell repeated `count` times.
    /// The inverse of [`from_cells`](Self::from_cells). The returned vector is
    /// allocated once, at the runs' total count.
    #[must_use]
    pub fn expand_cells(&self) -> Vec<FrameCell> {
        let total_cell_count = self
            .cell_runs
            .iter()
            .map(|cell_run| usize::from(cell_run.repeat_count))
            .sum();
        let mut frame_cells = Vec::with_capacity(total_cell_count);
        for cell_run in &self.cell_runs {
            let repeat_count = usize::from(cell_run.repeat_count);
            frame_cells.extend(std::iter::repeat_n(cell_run.cell.clone(), repeat_count));
        }
        frame_cells
    }
}

/// One run: how many times its cell repeats.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrameRun {
    /// How many cells this run stands for. Never 0.
    #[serde(rename = "count")]
    pub repeat_count: u16,
    /// The cell every position in the run holds.
    pub cell: FrameCell,
}

/// A single cell: its character, the rest of its grapheme cluster, its display
/// width, and its style.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrameCell {
    /// The base character occupying the cell.
    #[serde(rename = "ch")]
    pub character: char,
    /// The rest of the grapheme cluster layered over [`character`](Self::character), in
    /// arrival order: combining accents, variation selectors, and the joined
    /// parts of a multi-codepoint emoji. Empty for a plain cell; the client
    /// draws `character` followed by these as one glyph.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[serde(rename = "combining")]
    pub combining_characters: Vec<char>,
    /// Display width in cells: 0 (continuation half of a wide glyph), 1
    /// (narrow), or 2 (wide, e.g. CJK).
    #[serde(rename = "width")]
    pub cell_width: u8,
    /// The cell's colors and text attributes.
    pub style: FrameStyle,
}

/// A cell's colors and text attributes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrameStyle {
    /// The foreground color. A color this build has no name for reads as
    /// [`Default`](FrameColor::Default).
    #[serde(default, deserialize_with = "crate::wire::deserialize_or_default")]
    #[serde(rename = "fg")]
    pub foreground_color: FrameColor,
    /// The background color. A color this build has no name for reads as
    /// [`Default`](FrameColor::Default).
    #[serde(default, deserialize_with = "crate::wire::deserialize_or_default")]
    #[serde(rename = "bg")]
    pub background_color: FrameColor,
    /// The underline color (SGR 58); `None` follows the foreground color. A
    /// color this build has no name for reads as `None`.
    #[serde(
        default,
        deserialize_with = "crate::wire::deserialize_or_default",
        skip_serializing_if = "Option::is_none"
    )]
    pub underline_color: Option<FrameColor>,
    /// The boolean text attributes and the underline style.
    #[serde(rename = "attrs")]
    pub text_attributes: FrameAttrs,
}

/// The SGR text attributes of one cell.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrameAttrs {
    /// Bold / increased intensity (SGR 1).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    #[serde(rename = "bold")]
    pub is_bold: bool,
    /// Italic (SGR 3).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    #[serde(rename = "italic")]
    pub is_italic: bool,
    /// Reverse video — swap foreground and background (SGR 7).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    #[serde(rename = "reverse")]
    pub is_reverse: bool,
    /// Faint / decreased intensity (SGR 2).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    #[serde(rename = "faint")]
    pub is_faint: bool,
    /// Blink (SGR 5 slow or 6 rapid, collapsed to one flag).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    #[serde(rename = "blink")]
    pub is_blinking: bool,
    /// Conceal — hidden text (SGR 8).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    #[serde(rename = "conceal")]
    pub is_concealed: bool,
    /// Crossed-out / strikethrough (SGR 9).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    #[serde(rename = "strike")]
    pub is_struck_through: bool,
    /// Overline (SGR 53).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    #[serde(rename = "overline")]
    pub is_overlined: bool,
    /// The underline style (SGR 4 / 21 / 24 and the `4:n` forms). A style this
    /// build has no name for reads as [`None`](FrameUnderline::None).
    #[serde(default, deserialize_with = "crate::wire::deserialize_or_default")]
    #[serde(rename = "underline")]
    pub underline_style: FrameUnderline,
}

/// A foreground or background color. Mirrors `koshi_terminal::style::Color`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum FrameColor {
    /// The terminal's configured default color.
    #[default]
    Default,
    /// A 256-color palette index.
    Indexed(u8),
    /// A 24-bit truecolor value.
    Rgb(u8, u8, u8),
}

/// The underline style of a cell: a cell draws at most one underline. Mirrors
/// `koshi_terminal::style::UnderlineStyle`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum FrameUnderline {
    /// Not underlined (SGR 24 or `4:0`).
    #[default]
    None,
    /// Single underline (SGR 4 or `4:1`).
    Single,
    /// Double underline (SGR 21 or `4:2`).
    Double,
    /// Curly / wavy underline (`4:3`).
    Curly,
    /// Dotted underline (`4:4`).
    Dotted,
    /// Dashed underline (`4:5`).
    Dashed,
}

#[cfg(test)]
mod tests;
