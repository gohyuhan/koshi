//! One painted frame: the structure of the picture the session composed for
//! one attached client, at that client's own viewport and scroll position.
//!
//! The session solves the client's active tab and resolves that client's
//! highlight, then sends the result as the JSON part of a painted update
//! ([`painted_update`](crate::painted_update)): the layout, the tab bar, the
//! client's own state, and everything about each pane except its rows, its
//! cursor and its scroll position, which travel in the binary part. Every
//! attached client gets its own frame: a client on an 80×24 terminal and one
//! on a 200×50 terminal receive different frames of the same session in the
//! same instant.
//!
//! A field this build does not know is ignored, in this record and every one
//! under it: a frame from a newer koshi still draws. Image presentation fields
//! fall back to their plainest value when this build has no name for what
//! arrives: an unknown image protocol reads as Kitty, an unknown image action
//! as `Display`, and an unknown optional image dimension or Sixel background
//! reads as absent.

use koshi_core::geometry::{Rect, Size};
use koshi_core::ids::{ClientId, CommandId, PaneId, SessionId, TabId};
use koshi_core::lock::LockMode;
use koshi_core::mouse::MouseTracking;
use koshi_layout::mode::LayoutMode;
use koshi_layout::solver::StackHeader;
use serde::de::Error as _;
use serde::{Deserialize, Serialize};

const MAX_FRAME_IMAGE_SIDE_PIXEL_COUNT: u32 = 16_384;
const MAX_FRAME_IMAGE_PIXEL_COUNT: u64 = 16_777_216;

/// The most RGBA bytes the image records one connection's client holds may
/// total: 64 MiB. One image record is never larger.
pub const MAX_FRAME_IMAGE_TRANSFER_BYTE_COUNT: u64 = 67_108_864;

/// The largest raw image chunk carried by one image chunk frame
/// ([`image_chunk`](crate::image_chunk)).
pub const MAX_FRAME_IMAGE_CHUNK_BYTE_COUNT: usize = 1_048_576;

/// The most image transfers a client accepts after one painted update:
/// 4096.
pub const MAX_FRAME_IMAGE_TRANSFER_COUNT: usize = 4_096;

/// Decode an image presentation value through an owned JSON value, falling
/// back to `T::default()` when the value cannot be read. Reads from every
/// `serde_json` entry point, `serde_json::from_value` included.
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
/// and to its [`FrameFloatingPaneSlot`]s by [`PaneId`]: a slot says *where* a
/// pane sits, its [`FramePane`] says *what* is inside it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaintedFrame {
    /// The session being viewed: its identity, its solved active tab, its tab
    /// list, and the floating panes the viewing client shows.
    pub session_snapshot: FrameSession,
    /// Per-pane content: one entry per pane slot in the active tab and one per
    /// floating pane slot, matched to its slot by [`PaneId`].
    pub pane_snapshots: Vec<FramePane>,
    /// The viewing client's own state (viewport, focus, lock mode).
    pub client_snapshot: FrameClient,
}

/// The session-scoped part of a frame: identity, the solved active tab, the
/// entries the tab bar is drawn from, and the floating panes the viewing client
/// shows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrameSession {
    /// The session's stable id.
    pub session_id: SessionId,
    /// The session's display name.
    pub session_name: String,
    /// The tab this client is shown, solved and ready to draw.
    pub active_tab_snapshot: FrameTab,
    /// One entry per tab in the session, in display order.
    pub tab_snapshots: Vec<FrameTabMetadata>,
    /// The floating panes the viewing client shows, back to front: the pane
    /// shown longest first, the focused one last. Empty when the client has no
    /// floating viewport: it reports
    /// [`PaneArea::Starving`](koshi_core::geometry::PaneArea::Starving), or a
    /// pane area narrower than 5 columns or shorter than 6 rows.
    pub floating_pane_slots: Vec<FrameFloatingPaneSlot>,
    /// Every floating pane of the session, in the order the panes became
    /// floating, shown or minimized for the viewing client.
    pub floating_panes: Vec<FrameFloatingPane>,
}

/// One floating pane of the session, shown or minimized for the viewing
/// client. The frame carries each pane's title once: in the pane's
/// [`FramePane`] while the client shows the pane, and here while the client
/// minimized it.
///
/// A shown pane travels as `{"Shown": {"pane_id": …}}`, and a minimized pane
/// titled `htop` as `{"Minimized": {"pane_id": …, "pane_title": "htop"}}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum FrameFloatingPane {
    /// The viewing client shows the pane. The frame holds the pane's slot in
    /// [`floating_pane_slots`](FrameSession::floating_pane_slots) and its
    /// [`FramePane`], which holds its title. A client that loses its floating
    /// viewport has every pane it shows minimized by the session in the same
    /// change.
    Shown {
        /// The floating pane.
        pane_id: PaneId,
    },
    /// The viewing client minimized the pane. The frame holds no slot and no
    /// [`FramePane`] of it.
    Minimized {
        /// The floating pane.
        pane_id: PaneId,
        /// The pane's resolved display title, by the rule of
        /// [`FramePane::pane_title`]. `None` when the pane has reported
        /// neither a working directory nor a title.
        pane_title: Option<String>,
    },
}

/// The active tab, with its layout already solved into placed pane slots.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrameTab {
    /// The tab's stable id.
    pub tab_id: TabId,
    /// The tab's display name.
    pub tab_name: String,
    /// The solved layout: one [`FrameSlot`] per pane, giving outer and content
    /// rects and coarse status.
    pub pane_slots: Vec<FrameSlot>,
    /// The size the tab's layout was solved for: the per-axis minimum pane
    /// area across the clients viewing this tab. The
    /// [`pane_slots`](Self::pane_slots) rects live in this space with origin `(0, 0)`. A
    /// client whose own [`viewport_size`](FrameClient::viewport_size) is larger draws
    /// this layout centered and letterboxes the surrounding margin.
    pub tab_size: Size,
    /// Header strips for stacked panes: the one-row title bar each collapsed
    /// stack member shows in place of its content.
    pub stack_headers: Vec<StackHeader>,
    /// Whether this client sees the tab tiled, or sees a single pane zoomed to
    /// fill it. Zoom is per client: another client viewing the same tab in
    /// the same instant can carry a different value here.
    pub layout_mode: LayoutMode,
    /// True when the tab has no room to draw and every pane is suppressed;
    /// the client fills the whole frame with the "terminal too small" overlay.
    pub is_every_pane_suppressed: bool,
    /// Blank cells between two panes that meet along a horizontal or
    /// vertical split, in the [`pane_slots`](Self::pane_slots) space. A frame with
    /// no field, and a value that is not a cell count, read as `0`.
    #[serde(default, deserialize_with = "crate::wire::deserialize_or_default")]
    pub gap_cell_count: u16,
}

/// One tab's entry in the tab bar: enough to draw the tab list without its
/// layout.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrameTabMetadata {
    /// The tab's stable id.
    pub tab_id: TabId,
    /// The tab's display name.
    pub tab_name: String,
    /// The tab's position in the bar, starting at 0.
    pub tab_index: usize,
    /// Whether this is the client's active tab, drawn with the active marker.
    pub is_active: bool,
}

/// One pane's placement in the solved layout: where its box sits, its content
/// area, and coarse status flags. Paired with a [`FramePane`] by
/// [`pane_id`](Self::pane_id).
///
/// [`is_visible`](Self::is_visible) is true exactly when
/// [`content_rect`](Self::content_rect) is `Some`, and an
/// [`is_suppressed`](Self::is_suppressed) pane is not visible. `content_rect` is
/// `None` for three distinct reasons — no room,
/// hidden, or a collapsed stack member — and [`is_suppressed`](Self::is_suppressed)
/// marks the no-room case.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrameSlot {
    /// The pane this slot places.
    pub pane_id: PaneId,
    /// The outer pane box, including the 1-cell border gutter.
    pub outer_rect: Rect,
    /// The content area inside the border — the rect the PTY was sized from.
    /// `None` when the pane shows no content (suppressed, hidden, or a
    /// collapsed stack member). Cells and the cursor are drawn here.
    pub content_rect: Option<Rect>,
    /// Whether the pane is currently shown.
    pub is_visible: bool,
    /// Whether the pane is suppressed for lack of room.
    pub is_suppressed: bool,
}

/// One floating pane the viewing client shows, placed for that client.
///
/// The outer rect counts cells from the client's pane-area origin, with no
/// letterbox offset: an outer rect at column `150` draws at pane-area column
/// `150`, also when the client's tiled layout starts at column `60`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrameFloatingPaneSlot {
    /// The floating pane this slot places.
    pub pane_id: PaneId,
    /// The pane's box with its chrome: the border ring, the titlebar row and
    /// the separator row. The pane's cells and cursor are drawn inside the
    /// chrome, in a rect the size of the pane's PTY: the box less
    /// [`FLOATING_CHROME_SIZE`](koshi_core::constant::FLOATING_CHROME_SIZE),
    /// with its top-left cell one column right of and three rows below the
    /// box's top-left cell. Outer `(24, 8)` `72x22` → content `(25, 11)`
    /// `70x18`.
    pub outer_rect: Rect,
    /// Whether the viewing client pinned the pane at its cell.
    pub is_pinned: bool,
}

/// One image placement carried with a pane's visible cells.
///
/// `image_content_id` names an image record uploaded on this connection. A
/// client without that record still has the complete cell rectangle needed to
/// draw the unavailable-image marker.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FrameImagePlacement {
    /// The complete cell size and the clipped top and left cells.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cell_geometry: Option<koshi_core::geometry::ImageCellGeometry>,
    /// Record metadata for this placement of the shared pixel content.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image_record: Option<FrameImageRecordHeader>,
    /// The terminal-local placement identity.
    pub placement_id: u64,
    /// The connection-local image-record identity.
    pub image_content_id: u64,
    /// Whether the source snapshot has the named image record.
    pub is_available: bool,
    /// The zero-based row and column of the upper-left covered cell.
    pub anchor_cell: (u16, u16),
    /// The number of covered columns.
    pub column_count: u16,
    /// The number of covered rows.
    pub row_count: u16,
}

impl<'de> Deserialize<'de> for FrameImagePlacement {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct FrameImagePlacementFields {
            placement_id: u64,
            image_content_id: u64,
            #[serde(default)]
            cell_geometry: Option<koshi_core::geometry::ImageCellGeometry>,
            #[serde(default)]
            image_record: Option<FrameImageRecordHeader>,
            is_available: bool,
            anchor_cell: (u16, u16),
            column_count: u16,
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

/// Image metadata sent before its RGBA bytes arrive in chunks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrameImageRecordHeader {
    /// Protocol that supplied the image. An unknown protocol reads as Kitty.
    #[serde(default, deserialize_with = "deserialize_image_or_default")]
    pub protocol: FrameGraphicsProtocol,
    /// Image width in pixels.
    pub pixel_width: u32,
    /// Image height in pixels.
    pub pixel_height: u32,
    /// The state operation represented by the transfer. An unknown action reads
    /// as `Display`.
    #[serde(default, deserialize_with = "deserialize_image_or_default")]
    pub image_action: FrameImageAction,
    /// Display hints supplied by the protocol.
    pub display: FrameImageDisplay,
    /// Cursor position when the image sequence ended, as row and column.
    pub anchor_cell: (u16, u16),
}

/// One image record whose RGBA bytes follow in image chunk frames
/// ([`image_chunk`](crate::image_chunk)).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FrameImageTransfer {
    /// The image-record identity on this connection.
    pub image_content_id: u64,
    /// The image record metadata and its pixel dimensions.
    pub image_record: FrameImageRecordHeader,
    /// The exact number of RGBA bytes the chunks carry.
    pub image_byte_count: u64,
}

impl<'de> Deserialize<'de> for FrameImageTransfer {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct FrameImageTransferFields {
            image_content_id: u64,
            image_record: FrameImageRecordHeader,
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
    #[serde(skip_serializing_if = "is_zero_response_suppression_level")]
    pub response_suppression_level: u8,
    /// The requested width, if the sender supplied one. An unknown dimension is
    /// read as absent.
    #[serde(deserialize_with = "deserialize_image_or_default")]
    pub requested_width: Option<FrameImageDimension>,
    /// The requested height, if the sender supplied one. An unknown dimension is
    /// read as absent.
    #[serde(deserialize_with = "deserialize_image_or_default")]
    pub requested_height: Option<FrameImageDimension>,
    /// Whether the sender requests aspect-ratio preservation.
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
    pub is_unicode_placeholder: bool,
    /// The Kitty image z-index.
    pub z_index: i32,
    /// The parent Kitty image id for a relative placement.
    pub relative_image_id: Option<u32>,
    /// The parent Kitty placement id for a relative placement.
    pub relative_placement_id: Option<u32>,
    /// The horizontal cell offset from a relative parent placement.
    pub relative_column_offset: i32,
    /// The vertical cell offset from a relative parent placement.
    pub relative_row_offset: i32,
    /// The number of terminal columns requested by Kitty.
    pub requested_column_count: Option<u32>,
    /// The number of terminal rows requested by Kitty.
    pub requested_row_count: Option<u32>,
    /// The source image x offset requested by Kitty, in pixels.
    pub source_pixel_offset_x: Option<u32>,
    /// The source image y offset requested by Kitty, in pixels.
    pub source_pixel_offset_y: Option<u32>,
    /// The x offset inside the first terminal cell requested by Kitty.
    pub cell_pixel_offset_x: Option<u32>,
    /// The y offset inside the first terminal cell requested by Kitty.
    pub cell_pixel_offset_y: Option<u32>,
    /// Whether Kitty asks the placement to move the cursor after display.
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
pub(crate) fn compute_frame_image_byte_count(
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
    pub client_id: ClientId,
    /// The client's terminal size in cells.
    pub viewport_size: Size,
    /// The tab the client is viewing.
    pub active_tab_id: TabId,
    /// The pane that takes the client's input: its focused floating pane,
    /// else its focused pane in the active tab. `None` when the client focuses
    /// neither. The client highlights the pane whose [`FrameSlot::pane_id`]
    /// or [`FrameFloatingPaneSlot::pane_id`] matches, and places the cursor
    /// there.
    pub focused_pane_id: Option<PaneId>,
    /// The pane the client focused in its active tab, also while a focused
    /// floating pane takes its input. `None` when the client focuses no pane
    /// in that tab.
    pub active_tab_focused_pane_id: Option<PaneId>,
    /// The client's input mode, as the session has it.
    pub lock_mode: LockMode,
    /// The last lock command that the client's own connection sent and the
    /// session ran before it built this frame. `None` before the session runs
    /// one, and again after the session restarts.
    pub last_applied_lock_command_id: Option<CommandId>,
    /// The last floating pane move that the client's own connection sent, from
    /// its mouse or a key binding, and the session ran before it built this
    /// frame. `None` before the session runs one, and again after the session
    /// restarts.
    pub last_applied_floating_pane_move_command_id: Option<CommandId>,
    /// The last tiled pane placement that the client's own connection
    /// confirmed and the session ran before it built this frame. `None` before
    /// the session runs one, and again after the session restarts.
    pub last_applied_placement_command_id: Option<CommandId>,
    /// Whether this client grabs the mouse for text selection. Adds the
    /// `SELECT` tag to the mode indicator, and decides whether a press in a
    /// mouse-aware pane begins a highlight.
    pub is_mouse_selection_enabled: bool,
}

/// One pane's content apart from its rows, its cursor and its scroll
/// position: what is drawn inside the content rect of the matching
/// [`FrameSlot`] or [`FrameFloatingPaneSlot`] besides the cells, plus what a
/// mouse event over this pane is answered from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FramePane {
    /// The pane this content belongs to, matched to a [`FrameSlot`] or a
    /// [`FrameFloatingPaneSlot`] by id.
    pub pane_id: PaneId,
    /// The pane's resolved display title: on the alternate screen the running
    /// app's OSC 0/1/2 title; on the primary screen the shell's OSC 7 working
    /// directory (`~`-shortened), falling back to the OSC title. `None` when
    /// the pane has reported neither.
    pub pane_title: Option<String>,
    /// The complete image placements whose rectangles fit inside this pane's
    /// visible window. Empty when the pane has no image to draw.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub image_placement_snapshots: Vec<FrameImagePlacement>,
    /// Whether the whole screen is in reverse video (DECSCNM): the client swaps
    /// the default foreground and background for every cell.
    pub is_reverse_video: bool,
    /// Which mouse events the pane's program asked to be told about
    /// (`?9`/`?1000`/`?1002`/`?1003`). Present in every frame: a pane that
    /// asked for nothing sends [`MouseTracking::Off`].
    pub mouse_tracking: MouseTracking,
    /// Whether alternate-scroll mode (`?1007`) is on: on the alternate screen a
    /// wheel tick becomes cursor arrow keys.
    pub is_alternate_scroll_enabled: bool,
    /// Whether the pane is showing the alternate screen. The alternate screen
    /// keeps no scrollback and has no view to scroll.
    pub is_on_alt_screen: bool,
    /// The viewing client's highlighted text in this pane, cut down to the rows
    /// this frame shows. `None` when the client has nothing highlighted here,
    /// or when the highlight is entirely outside the visible rows.
    pub selection_spans: Option<FrameSelection>,
    /// Whether the viewing client has a highlight in this pane at all,
    /// including one scrolled entirely out of the visible rows, where
    /// [`selection_spans`](Self::selection_spans) is `None`.
    pub has_selection: bool,
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
    pub row_spans: Vec<(u16, u16, u16)>,
}

#[cfg(test)]
pub(crate) mod tests;
