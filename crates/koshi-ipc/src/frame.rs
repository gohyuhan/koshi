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
//! drawn from — [`truncated`](crate::frame::FrameScrollback::truncated) and
//! [`retained_lines`](crate::frame::FrameScrollback::retained_lines). A client
//! scrolled 500 lines back over a 24-row pane receives those 24 rows, never the
//! 500 above them.
//!
//! Rows are run-length encoded.
//! [`FrameRow::from_cells`](crate::frame::FrameRow::from_cells) folds each
//! stretch of equal neighbouring cells into one
//! [`FrameRun`](crate::frame::FrameRun): a blank 80-column row travels as a
//! single run with `count == 80`, and
//! [`FrameRow::cells`](crate::frame::FrameRow::cells) expands the runs back
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

const MAX_FRAME_IMAGE_SIDE: u32 = 16_384;
const MAX_FRAME_IMAGE_PIXELS: u64 = 16_777_216;

/// The maximum total RGBA bytes a chunked painted frame may carry.
pub const MAX_FRAME_IMAGE_TRANSFER_BYTES: u64 = 67_108_864;

/// The largest raw image chunk carried by one session event.
pub const MAX_FRAME_IMAGE_CHUNK_BYTES: usize = 1_048_576;

/// The largest number of image transfers accepted after one painted frame.
pub const MAX_FRAME_IMAGE_TRANSFERS: usize = 4_096;

/// Decode an image presentation value through an owned JSON value so the
/// fallback works for transport input and for owned deserialization callers.
fn image_or_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::de::DeserializeOwned + Default,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(T::deserialize(value).unwrap_or_default())
}

/// One frame, as it travels to a client: the session's active tab, the content
/// of every pane in it, and the viewing client's own state.
///
/// A reader joins [`panes`](Self::panes) to the [`FrameSlot`]s in
/// [`session`](Self::session)'s active tab by [`PaneId`]: a slot says *where* a
/// pane sits, its [`FramePane`] says *what* is inside it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaintedFrame {
    /// The session being viewed: its identity, its solved active tab, and its
    /// tab list.
    pub session: FrameSession,
    /// Per-pane content, one entry per live pane in the active tab, matched to
    /// a [`FrameSlot`] by [`PaneId`].
    pub panes: Vec<FramePane>,
    /// The viewing client's own state (viewport, focus, lock mode).
    pub client: FrameClient,
}

/// The session-scoped part of a frame: identity, the solved active tab, and the
/// entries the tab bar is drawn from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrameSession {
    /// The session's stable id.
    pub id: SessionId,
    /// The session's display name.
    pub name: String,
    /// The tab this client is shown, solved and ready to draw.
    pub active_tab: FrameTab,
    /// One entry per tab in the session, in display order.
    pub tabs: Vec<FrameTabMeta>,
}

/// The active tab, with its layout already solved into placed pane slots.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrameTab {
    /// The tab's stable id.
    pub id: TabId,
    /// The tab's display name.
    pub name: String,
    /// The solved layout: one [`FrameSlot`] per pane, giving outer and content
    /// rects and coarse status.
    pub slots: Vec<FrameSlot>,
    /// The viewport size the layout was solved for: the element-wise minimum
    /// viewport across the clients viewing this tab. The
    /// [`slots`](Self::slots) rects live in this space with origin `(0, 0)`. A
    /// client whose own [`viewport`](FrameClient::viewport) is larger draws
    /// this layout centered and letterboxes the surrounding margin.
    pub effective_size: Size,
    /// Header strips for stacked panes: the one-row title bar each collapsed
    /// stack member shows in place of its content.
    pub stack_headers: Vec<StackHeader>,
    /// Whether this client sees the tab tiled, or sees a single pane zoomed to
    /// fill it. Zoom is per client: another client viewing the same tab in
    /// the same instant can carry a different value here.
    pub layout_mode: LayoutMode,
    /// True when the tab has no room to draw and every pane is suppressed;
    /// the client fills the whole frame with the "terminal too small" overlay.
    pub all_suppressed: bool,
    /// Blank cells between two panes that meet along a horizontal or
    /// vertical split, in the [`slots`](Self::slots) space. A frame from a
    /// server without this field reads as `0`, and so does a value that is
    /// not a cell count.
    #[serde(default, deserialize_with = "crate::wire::or_default")]
    pub gap: u16,
}

/// One tab's entry in the tab bar: enough to draw the tab list without its
/// layout.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrameTabMeta {
    /// The tab's stable id.
    pub id: TabId,
    /// The tab's display name.
    pub name: String,
    /// The tab's position in the bar, starting at 0.
    pub index: usize,
    /// Whether this is the client's active tab, drawn with the active marker.
    pub active: bool,
}

/// One pane's placement in the solved layout: where its box sits, its content
/// area, and coarse status flags. Paired with a [`FramePane`] by
/// [`pane_id`](Self::pane_id).
///
/// [`visible`](Self::visible) is true exactly when
/// [`inner_rect`](Self::inner_rect) is `Some`, and a
/// [`suppressed`](Self::suppressed) pane is not visible. [`dead`](Self::dead)
/// is a separate axis: an exited pane stays laid out, drawn dimmed, until it is
/// removed. `inner_rect` is `None` for three distinct reasons — no room,
/// hidden, or a collapsed stack member — and [`suppressed`](Self::suppressed)
/// marks the no-room case.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrameSlot {
    /// The pane this slot places.
    pub pane_id: PaneId,
    /// The outer pane box, including the 1-cell border gutter.
    pub rect: Rect,
    /// The content area inside the border — the rect the PTY was sized from.
    /// `None` when the pane shows no content (suppressed, hidden, or a
    /// collapsed stack member). Cells and the cursor are drawn here.
    pub inner_rect: Option<Rect>,
    /// Whether a terminal or a plugin backs this pane.
    pub kind: PaneKind,
    /// Whether the pane is currently shown.
    pub visible: bool,
    /// Whether the pane is suppressed for lack of room.
    pub suppressed: bool,
    /// Whether the pane's process has exited.
    pub dead: bool,
}

/// One image placement carried with a pane's visible cells.
///
/// `content_id` names an image record uploaded on this connection. A client
/// without that record still has the complete cell rectangle needed to draw
/// the unavailable-image marker.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FrameImagePlacement {
    /// The terminal-local placement identity.
    pub id: u64,
    /// The connection-local image-record identity.
    pub content_id: u64,
    /// Whether the source snapshot has the named image record.
    pub available: bool,
    /// The zero-based row and column of the upper-left covered cell.
    pub anchor: (u16, u16),
    /// The number of covered columns.
    pub columns: u16,
    /// The number of covered rows.
    pub rows: u16,
}

impl<'de> Deserialize<'de> for FrameImagePlacement {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct Fields {
            id: u64,
            content_id: u64,
            #[serde(default = "default_image_available")]
            available: bool,
            anchor: (u16, u16),
            columns: u16,
            rows: u16,
        }

        let fields = Fields::deserialize(deserializer)?;
        if fields.id == 0 {
            return Err(D::Error::custom(
                "image placement identity must not be zero",
            ));
        }
        if fields.content_id == 0 {
            return Err(D::Error::custom("image content identity must not be zero"));
        }
        if fields.columns == 0 || fields.rows == 0 {
            return Err(D::Error::custom(
                "image placement dimensions must not be zero",
            ));
        }
        let coordinate_end = u32::from(u16::MAX) + 1;
        if u32::from(fields.anchor.0) + u32::from(fields.rows) > coordinate_end
            || u32::from(fields.anchor.1) + u32::from(fields.columns) > coordinate_end
        {
            return Err(D::Error::custom(
                "image placement exceeds the cell coordinate range",
            ));
        }
        Ok(Self {
            id: fields.id,
            content_id: fields.content_id,
            available: fields.available,
            anchor: fields.anchor,
            columns: fields.columns,
            rows: fields.rows,
        })
    }
}

const fn default_image_available() -> bool {
    true
}

/// Image metadata sent before its RGBA bytes arrive in chunks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrameImageRecordHeader {
    /// Protocol that supplied the image. An unknown protocol reads as Kitty.
    #[serde(default, deserialize_with = "image_or_default")]
    pub protocol: FrameGraphicsProtocol,
    /// Image width in pixels.
    pub width: u32,
    /// Image height in pixels.
    pub height: u32,
    /// The state operation represented by the transfer. An unknown action reads
    /// as `Display`.
    #[serde(default, deserialize_with = "image_or_default")]
    pub action: FrameImageAction,
    /// Display hints supplied by the protocol.
    pub display: FrameImageDisplay,
    /// Cursor position when the image sequence ended, as row and column.
    pub anchor: (u16, u16),
}

/// One image record whose RGBA bytes travel in separate bounded events.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FrameImageTransfer {
    /// The image-record identity on this connection.
    pub id: u64,
    /// The image record metadata and its pixel dimensions.
    pub record: FrameImageRecordHeader,
    /// The exact number of RGBA bytes the chunks carry.
    pub byte_len: u64,
}

impl<'de> Deserialize<'de> for FrameImageTransfer {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct Fields {
            id: u64,
            record: FrameImageRecordHeader,
            byte_len: u64,
        }

        let fields = Fields::deserialize(deserializer)?;
        if fields.id == 0 {
            return Err(D::Error::custom("image transfer identity must not be zero"));
        }
        if fields.record.action == FrameImageAction::Transmit {
            return Err(D::Error::custom(
                "a transmitted-only image cannot be an image placement",
            ));
        }
        let expected_bytes = frame_image_byte_len(fields.record.width, fields.record.height)
            .map_err(D::Error::custom)?;
        if fields.byte_len != expected_bytes {
            return Err(D::Error::custom(
                "image transfer byte length does not match its dimensions",
            ));
        }
        Ok(Self {
            id: fields.id,
            record: fields.record,
            byte_len: fields.byte_len,
        })
    }
}

/// One bounded part of one chunked image transfer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FrameImageChunk {
    /// The image-record identity named by the transfer start.
    pub transfer_id: u64,
    /// The raw-byte offset of `bytes` in that image.
    pub offset: u64,
    /// Whether this chunk ends the transfer.
    pub last: bool,
    /// Raw RGBA bytes, encoded as base64 on the wire.
    #[serde(with = "crate::bytes::base64_or_list")]
    pub bytes: Vec<u8>,
}

impl<'de> Deserialize<'de> for FrameImageChunk {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct Fields {
            transfer_id: u64,
            offset: u64,
            last: bool,
            #[serde(with = "crate::bytes::base64_or_list")]
            bytes: Vec<u8>,
        }

        let fields = Fields::deserialize(deserializer)?;
        if fields.transfer_id == 0 {
            return Err(D::Error::custom("image transfer identity must not be zero"));
        }
        if fields.bytes.is_empty() {
            return Err(D::Error::custom("image chunk must not be empty"));
        }
        if fields.bytes.len() > MAX_FRAME_IMAGE_CHUNK_BYTES {
            return Err(D::Error::custom("image chunk exceeds its byte limit"));
        }
        Ok(Self {
            transfer_id: fields.transfer_id,
            offset: fields.offset,
            last: fields.last,
            bytes: fields.bytes,
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
    /// The requested width, if the sender supplied one. An unknown dimension is
    /// read as absent.
    #[serde(deserialize_with = "image_or_default")]
    pub width: Option<FrameImageDimension>,
    /// The requested height, if the sender supplied one. An unknown dimension is
    /// read as absent.
    #[serde(deserialize_with = "image_or_default")]
    pub height: Option<FrameImageDimension>,
    /// Whether the sender requests aspect-ratio preservation.
    pub preserve_aspect_ratio: bool,
    /// The Sixel background rule, when the record came from Sixel. An unknown
    /// rule is read as absent.
    #[serde(deserialize_with = "image_or_default")]
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
    pub unicode_placeholder: bool,
    /// The Kitty image z-index.
    pub z_index: i32,
    /// The number of terminal columns requested by Kitty.
    pub cell_columns: Option<u32>,
    /// The number of terminal rows requested by Kitty.
    pub cell_rows: Option<u32>,
    /// The source image x offset requested by Kitty, in pixels.
    pub source_offset_x: Option<u32>,
    /// The source image y offset requested by Kitty, in pixels.
    pub source_offset_y: Option<u32>,
    /// The x offset inside the first terminal cell requested by Kitty.
    pub cell_offset_x: Option<u32>,
    /// The y offset inside the first terminal cell requested by Kitty.
    pub cell_offset_y: Option<u32>,
    /// Whether Kitty asks the placement to move the cursor after display.
    pub move_cursor: bool,
}

impl Default for FrameImageDisplay {
    fn default() -> Self {
        Self {
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
fn frame_image_byte_len(width: u32, height: u32) -> Result<u64, &'static str> {
    let pixels = u64::from(width)
        .checked_mul(u64::from(height))
        .ok_or("image dimensions overflow")?;
    let expected_bytes = pixels.checked_mul(4).ok_or("image byte count overflows")?;
    if width == 0
        || height == 0
        || width > MAX_FRAME_IMAGE_SIDE
        || height > MAX_FRAME_IMAGE_SIDE
        || pixels > MAX_FRAME_IMAGE_PIXELS
        || expected_bytes > MAX_FRAME_IMAGE_TRANSFER_BYTES
    {
        return Err("image dimensions exceed graphics limits");
    }
    Ok(expected_bytes)
}

/// The viewing client's own state: what this client sees and how it is moded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrameClient {
    /// The client's stable id.
    pub id: ClientId,
    /// The client's terminal size in cells.
    pub viewport: Size,
    /// The tab the client is viewing.
    pub active_tab: TabId,
    /// The client's focused pane in the active tab, or `None` when the tab has
    /// no focusable pane. The client highlights the pane whose
    /// [`FrameSlot::pane_id`] matches, and places the cursor there.
    pub focused_pane: Option<PaneId>,
    /// The client's input mode, as the session has it.
    pub lock_mode: LockMode,
    /// Whether this client grabs the mouse for text selection. Adds the
    /// `SELECT` tag to the mode indicator, and decides whether a press in a
    /// mouse-aware pane begins a highlight.
    pub mouse_select: bool,
}

/// One pane's content: the cells drawn inside the matching [`FrameSlot`]'s
/// content rect, plus what a mouse event over this pane is answered from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FramePane {
    /// The pane this content belongs to, matched to a [`FrameSlot`] by id.
    pub id: PaneId,
    /// The pane's resolved display title: on the alternate screen the running
    /// app's OSC 0/1/2 title; on the primary screen the shell's OSC 7 working
    /// directory (`~`-shortened), falling back to the OSC title. `None` when
    /// the pane has reported neither.
    pub title: Option<String>,
    /// The cursor's position and look within the content area.
    pub cursor: FrameCursor,
    /// The visible terminal cells. `None` for a pane with no terminal content —
    /// a plugin pane, or a slot showing nothing this frame.
    pub window: Option<FrameWindow>,
    /// The complete image placements whose rectangles fit inside this pane's
    /// visible window. Empty when the pane has no image to draw.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub image_placements: Vec<FrameImagePlacement>,
    /// Whether the whole screen is in reverse video (DECSCNM): the client swaps
    /// the default foreground and background for every cell.
    pub reverse_video: bool,
    /// Which mouse events the pane's program asked to be told about
    /// (`?9`/`?1000`/`?1002`/`?1003`). Present in every frame: a pane that
    /// asked for nothing sends [`MouseTracking::Off`].
    pub mouse_tracking: MouseTracking,
    /// Whether alternate-scroll mode (`?1007`) is on: on the alternate screen a
    /// wheel tick becomes cursor arrow keys.
    pub alt_scroll: bool,
    /// Whether the pane is showing the alternate screen. The alternate screen
    /// keeps no scrollback and has no view to scroll.
    pub on_alt_screen: bool,
    /// The absolute line number of the top row this frame shows for the pane,
    /// counting every line the pane has ever pushed into scrollback. A press on
    /// the pane's `n`-th visible row names line `view_top_row + n`.
    pub view_top_row: u64,
    /// The viewing client's highlighted text in this pane, cut down to the rows
    /// this frame shows. `None` when the client has nothing highlighted here,
    /// or when the highlight is entirely outside the visible rows.
    pub selection: Option<FrameSelection>,
    /// Whether the viewing client has a highlight in this pane at all,
    /// including one scrolled entirely out of the visible rows, where
    /// [`selection`](Self::selection) is `None`.
    pub has_selection: bool,
    /// Scrollback state for the scroll-position indicator.
    pub scrollback: FrameScrollback,
}

/// The cursor's position within the content area, and how it is drawn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrameCursor {
    /// The cursor's row within the content area, starting at 0.
    pub row: u16,
    /// The cursor's column within the content area, starting at 0.
    pub col: u16,
    /// Whether the cursor is visible.
    pub visible: bool,
    /// Whether the cursor blinks.
    pub blink: bool,
    /// The shape the cursor is drawn as (DECSCUSR), or `None` while the pane
    /// has asked for no shape at all, which leaves the user's own configured
    /// cursor standing. A shape this build has no name for reads as `None`.
    #[serde(default, deserialize_with = "crate::wire::or_default")]
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
    pub rows: Vec<(u16, u16, u16)>,
}

/// Scrollback state the scroll-position indicator is drawn from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrameScrollback {
    /// Whether the buffer reached its cap and dropped its oldest lines.
    pub truncated: bool,
    /// How many scrollback lines are currently retained.
    pub retained_lines: usize,
}

/// The cells a pane shows this frame, run-length encoded row by row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrameWindow {
    /// The width every row expands back to.
    pub cols: u16,
    /// The visible rows, top row first.
    pub rows: Vec<FrameRow>,
    /// Rows scrolled up from the live tail; `0` shows the live bottom of the
    /// buffer.
    pub view_offset: usize,
}

/// One row of cells, as runs of equal neighbouring cells.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrameRow {
    /// The runs, left to right. Their counts sum to the row's width.
    pub runs: Vec<FrameRun>,
    /// Whether the row ends its logical line or continues onto the next. An
    /// ending this build has no name for reads as
    /// [`Hard`](FrameRowEnd::Hard).
    #[serde(
        default,
        deserialize_with = "crate::wire::or_default",
        skip_serializing_if = "FrameRowEnd::is_hard"
    )]
    pub end: FrameRowEnd,
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
    /// Fold `cells` into runs: each stretch of equal neighbouring cells becomes
    /// one [`FrameRun`]. A count stops at [`u16::MAX`] and the next equal cell
    /// opens a new run. `end` is how the row ends its logical line.
    ///
    /// 80 blank cells give one run with `count == 80`. 70 000 blank cells give
    /// two runs, `65_535` then `4_465`.
    #[must_use]
    pub fn from_cells(cells: impl IntoIterator<Item = FrameCell>, end: FrameRowEnd) -> Self {
        let mut runs: Vec<FrameRun> = Vec::new();
        for cell in cells {
            match runs.last_mut() {
                Some(run) if run.cell == cell && run.count < u16::MAX => run.count += 1,
                _ => runs.push(FrameRun { count: 1, cell }),
            }
        }
        Self { runs, end }
    }

    /// Expand the runs back into cells, each run's cell repeated `count` times.
    /// The inverse of [`from_cells`](Self::from_cells). The returned vector is
    /// allocated once, at the runs' total count.
    #[must_use]
    pub fn cells(&self) -> Vec<FrameCell> {
        let total = self.runs.iter().map(|run| usize::from(run.count)).sum();
        let mut cells = Vec::with_capacity(total);
        for run in &self.runs {
            let count = usize::from(run.count);
            cells.extend(std::iter::repeat_n(run.cell.clone(), count));
        }
        cells
    }
}

/// One run: how many times its cell repeats.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrameRun {
    /// How many cells this run stands for. Never 0.
    pub count: u16,
    /// The cell every position in the run holds.
    pub cell: FrameCell,
}

/// A single cell: its character, the rest of its grapheme cluster, its display
/// width, and its style.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrameCell {
    /// The base character occupying the cell.
    pub ch: char,
    /// The rest of the grapheme cluster layered over [`ch`](Self::ch), in
    /// arrival order: combining accents, variation selectors, and the joined
    /// parts of a multi-codepoint emoji. Empty for a plain cell; the client
    /// draws `ch` followed by these as one glyph.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub combining: Vec<char>,
    /// Display width in cells: 0 (continuation half of a wide glyph), 1
    /// (narrow), or 2 (wide, e.g. CJK).
    pub width: u8,
    /// The cell's colors and text attributes.
    pub style: FrameStyle,
}

/// A cell's colors and text attributes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrameStyle {
    /// The foreground color. A color this build has no name for reads as
    /// [`Default`](FrameColor::Default).
    #[serde(default, deserialize_with = "crate::wire::or_default")]
    pub fg: FrameColor,
    /// The background color. A color this build has no name for reads as
    /// [`Default`](FrameColor::Default).
    #[serde(default, deserialize_with = "crate::wire::or_default")]
    pub bg: FrameColor,
    /// The underline color (SGR 58); `None` follows the foreground color. A
    /// color this build has no name for reads as `None`.
    #[serde(
        default,
        deserialize_with = "crate::wire::or_default",
        skip_serializing_if = "Option::is_none"
    )]
    pub underline_color: Option<FrameColor>,
    /// The boolean text attributes and the underline style.
    pub attrs: FrameAttrs,
}

/// The SGR text attributes of one cell.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrameAttrs {
    /// Bold / increased intensity (SGR 1).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub bold: bool,
    /// Italic (SGR 3).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub italic: bool,
    /// Reverse video — swap foreground and background (SGR 7).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub reverse: bool,
    /// Faint / decreased intensity (SGR 2).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub faint: bool,
    /// Blink (SGR 5 slow or 6 rapid, collapsed to one flag).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub blink: bool,
    /// Conceal — hidden text (SGR 8).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub conceal: bool,
    /// Crossed-out / strikethrough (SGR 9).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub strike: bool,
    /// Overline (SGR 53).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub overline: bool,
    /// The underline style (SGR 4 / 21 / 24 and the `4:n` forms). A style this
    /// build has no name for reads as [`None`](FrameUnderline::None).
    #[serde(default, deserialize_with = "crate::wire::or_default")]
    pub underline: FrameUnderline,
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
