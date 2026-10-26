//! Terminal-image geometry and the two client paint modes.
//!
//! A snapshot carries a cell rectangle and may carry its complete image
//! record. This module maps that rectangle into the committed pane area, clips
//! it to the pane and frame, and records the matching source-pixel rectangle.
//! A client that cannot emit an image protocol paints every rectangle with the
//! fixed `terminal image unavailable` text. A native-image client paints the
//! same text while a record is arriving and keeps ordinary cells beneath a
//! complete image so transparent and negative-z pixels compose correctly.

use std::sync::Arc;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect as RatatuiRect;
use ratatui::style::Style;

use koshi_core::ids::PaneId;
use koshi_terminal::graphics::{GraphicsProtocol, ImageRecord};
use koshi_terminal::state::ImagePlacementId;
use koshi_terminal::style::Style as CellStyle;

use crate::render::{list_visible_tiled_panes, FrameGeometry, PlacedFloatingPane};
use crate::snapshot::{
    ImagePlacementSnapshot, PanePlacementPresentation, PaneSnapshot, RenderSnapshot,
};

/// The text a client paints when it cannot display terminal image pixels.
pub const TERMINAL_IMAGE_UNAVAILABLE: &str = "terminal image unavailable";

/// The largest shared cell snapshot used to classify image composition.
pub const MAX_IMAGE_CELL_SNAPSHOT_CELL_COUNT: usize = 262_144;

/// The identity of one image placement in a rendered pane.
pub type ImagePlacementKey = (PaneId, ImagePlacementId);

/// The cell facts needed to classify image composition.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ImageCellState {
    /// The base character in the cell.
    pub character: char,
    /// The terminal display width of the cell.
    pub cell_width: u8,
    /// The combining and joined code points after the base character.
    pub combining_characters: Vec<char>,
    /// The terminal style applied to the cell.
    pub style: CellStyle,
}

impl Default for ImageCellState {
    fn default() -> Self {
        Self {
            character: ' ',
            cell_width: 1,
            combining_characters: Vec::new(),
            style: CellStyle::default(),
        }
    }
}

/// One bounded row-major snapshot of rendered cell facts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageCellSnapshot {
    /// The absolute frame area represented by `cell_states`.
    pub screen_area: RatatuiRect,
    cell_states: Vec<ImageCellState>,
}

impl ImageCellSnapshot {
    /// Build a row-major cell snapshot when the area and cell count match.
    #[must_use]
    pub fn from_cell_states(
        screen_area: RatatuiRect,
        cell_states: Vec<ImageCellState>,
    ) -> Option<Self> {
        let expected_cell_count =
            usize::from(screen_area.width).checked_mul(usize::from(screen_area.height))?;
        if expected_cell_count > MAX_IMAGE_CELL_SNAPSHOT_CELL_COUNT
            || cell_states.len() != expected_cell_count
        {
            return None;
        }
        Some(Self {
            screen_area,
            cell_states,
        })
    }

    /// Return the cell at an absolute frame position.
    #[must_use]
    pub fn find_cell(&self, screen_column: u16, screen_row: u16) -> Option<&ImageCellState> {
        if screen_column < self.screen_area.x
            || screen_row < self.screen_area.y
            || screen_column >= self.screen_area.right()
            || screen_row >= self.screen_area.bottom()
        {
            return None;
        }
        let row_offset = usize::from(screen_row - self.screen_area.y);
        let column_offset = usize::from(screen_column - self.screen_area.x);
        let cell_index = row_offset
            .checked_mul(usize::from(self.screen_area.width))?
            .checked_add(column_offset)?;
        self.cell_states.get(cell_index)
    }
}

/// Snapshot the rendered cell facts of the frame `frame_geometry` draws in its
/// viewport area, over the cells of `snapshot_area` inside it: the cells of
/// every visible tiled pane, then those of each floating pane this viewer
/// draws, back to front. A floating pane first resets every cell of its outer
/// area to a blank cell, then writes the cells of its visible content area.
/// The viewer chrome decides which floating panes are drawn, as it does for
/// [`render_frame`](crate::render_frame). The snapshot's `screen_area` is
/// `snapshot_area` clipped to the viewport area, and each of its cells holds
/// the fact a snapshot of the whole viewport holds at that cell. Returns `None`
/// when that area holds more than [`MAX_IMAGE_CELL_SNAPSHOT_CELL_COUNT`] cells
/// or their memory cannot be reserved.
///
/// Example: a 200×60 viewport and a `snapshot_area` of 40×20 cells at
/// `(10, 10)` give an 800-cell snapshot whose cell `(10, 10)` is the frame's
/// cell `(10, 10)`.
pub fn build_image_cell_snapshot(
    frame_geometry: &FrameGeometry<'_>,
    snapshot_area: RatatuiRect,
) -> Option<ImageCellSnapshot> {
    let render_snapshot = frame_geometry.render_snapshot;
    let screen_area = snapshot_area.intersection(frame_geometry.viewport_area);
    let cell_count = usize::from(screen_area.width).checked_mul(usize::from(screen_area.height))?;
    if cell_count > MAX_IMAGE_CELL_SNAPSHOT_CELL_COUNT {
        return None;
    }
    let mut cell_states = Vec::new();
    cell_states.try_reserve_exact(cell_count).ok()?;
    cell_states.resize(cell_count, ImageCellState::default());
    if cell_count == 0 {
        return Some(ImageCellSnapshot {
            screen_area,
            cell_states,
        });
    }

    for (pane_snapshot, placed_content_rect) in
        list_visible_tiled_panes(render_snapshot, frame_geometry.get_layout_origin())
    {
        write_pane_cell_states(
            pane_snapshot,
            placed_content_rect,
            placed_content_rect,
            screen_area,
            &mut cell_states,
        )?;
    }
    for placed_floating_pane in &frame_geometry.placed_floating_panes {
        let outer_cell_area = placed_floating_pane.outer_area.intersection(screen_area);
        for screen_row in outer_cell_area.top()..outer_cell_area.bottom() {
            for screen_column in outer_cell_area.left()..outer_cell_area.right() {
                let cell_index = compute_cell_index(screen_area, screen_column, screen_row)?;
                cell_states[cell_index] = ImageCellState::default();
            }
        }
        let Some(pane_snapshot) =
            render_snapshot.find_pane_snapshot(placed_floating_pane.floating_pane_slot.pane_id)
        else {
            continue;
        };
        write_pane_cell_states(
            pane_snapshot,
            placed_floating_pane.content_area,
            placed_floating_pane.visible_content_area,
            screen_area,
            &mut cell_states,
        )?;
    }
    Some(ImageCellSnapshot {
        screen_area,
        cell_states,
    })
}

/// Write the cell facts of `pane_snapshot`'s grid into `cell_states`, the
/// row-major cells of `screen_area`, over the cells of `clip_area` inside
/// `screen_area`. The grid's first cell sits at `content_area`'s top-left
/// cell. A cell past the grid's last row or column keeps its fact, and a pane
/// with no terminal grid writes nothing. Returns `None` only when a cell index
/// does not fit in `usize`.
fn write_pane_cell_states(
    pane_snapshot: &PaneSnapshot,
    content_area: RatatuiRect,
    clip_area: RatatuiRect,
    screen_area: RatatuiRect,
    cell_states: &mut [ImageCellState],
) -> Option<()> {
    let Some(grid_view) = &pane_snapshot.terminal_grid_view else {
        return Some(());
    };
    let cell_area = clip_area.intersection(screen_area);
    for row_offset in 0..cell_area.height {
        let screen_row = cell_area.y + row_offset;
        let grid_row_index = screen_row.saturating_sub(content_area.y);
        for column_offset in 0..cell_area.width {
            let screen_column = cell_area.x + column_offset;
            let grid_column_index = screen_column.saturating_sub(content_area.x);
            let Some(cell) = grid_view.get_cell(grid_row_index, grid_column_index) else {
                continue;
            };
            let cell_index = compute_cell_index(screen_area, screen_column, screen_row)?;
            let is_selected = pane_snapshot
                .selection_spans
                .as_ref()
                .and_then(|selection_spans| selection_spans.find_row_span(grid_row_index))
                .is_some_and(|(start_grid_column, end_grid_column)| {
                    grid_column_index >= start_grid_column && grid_column_index <= end_grid_column
                });
            let mut cell_style = cell.get_style();
            cell_style.set_reverse(
                cell_style.get_attributes().is_reverse()
                    ^ pane_snapshot.is_reverse_video
                    ^ is_selected,
            );
            cell_states[cell_index] = ImageCellState {
                character: cell.get_character(),
                cell_width: cell.get_display_width(),
                combining_characters: cell.list_combining_characters().to_vec(),
                style: cell_style,
            };
        }
    }
    Some(())
}

/// The row-major index of the frame cell `(screen_column, screen_row)` among
/// the cells of `screen_area`, which holds that cell. Returns `None` only when
/// the index does not fit in `usize`.
fn compute_cell_index(
    screen_area: RatatuiRect,
    screen_column: u16,
    screen_row: u16,
) -> Option<usize> {
    usize::from(screen_row - screen_area.y)
        .checked_mul(usize::from(screen_area.width))?
        .checked_add(usize::from(screen_column - screen_area.x))
}

/// The image output capability selected for one attached terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageRenderMode {
    /// Paint image rectangles with the unavailable-image text.
    Placeholder,
    /// Keep prepared image cells unchanged for native protocol output.
    Native,
}

/// A source-pixel rectangle paired with one clipped destination rectangle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageSourceRect {
    /// The source x coordinate in pixels.
    pub pixel_x: u32,
    /// The source y coordinate in pixels.
    pub pixel_y: u32,
    /// The source width in pixels.
    pub pixel_width: u32,
    /// The source height in pixels.
    pub pixel_height: u32,
}

/// One image that can be painted inside a committed pane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImagePaint {
    /// The pane that owns the image.
    pub pane_id: PaneId,
    /// The terminal-local image placement identity.
    pub placement_id: ImagePlacementId,
    /// The connection-local image-record identity.
    pub image_content_id: u64,
    /// The complete image record, including row-major RGBA pixels.
    pub image_record: Arc<ImageRecord>,
    /// The destination cells after pane and frame clipping.
    pub target_area: RatatuiRect,
    /// The source pixels that map to `target_area`.
    pub source_rect: ImageSourceRect,
    /// The Kitty x offset inside the first destination cell.
    pub cell_pixel_offset_x: Option<u32>,
    /// The Kitty y offset inside the first destination cell.
    pub cell_pixel_offset_y: Option<u32>,
    /// The protocol z-index used to order overlaps.
    pub z_index: i32,
    draw_order: usize,
}

impl ImagePaint {
    /// Build one image paint of the record `image_content_id` names, from its
    /// clipped target and source rectangles.
    #[must_use]
    pub fn from_image_placement(
        pane_id: PaneId,
        placement_id: ImagePlacementId,
        image_content_id: u64,
        image_record: Arc<ImageRecord>,
        target_area: RatatuiRect,
        source_rect: ImageSourceRect,
        z_index: i32,
    ) -> Self {
        let is_kitty_protocol = image_record.protocol == GraphicsProtocol::Kitty;
        let cell_pixel_offset_x = is_kitty_protocol
            .then_some(image_record.display.cell_pixel_offset_x)
            .flatten();
        let cell_pixel_offset_y = is_kitty_protocol
            .then_some(image_record.display.cell_pixel_offset_y)
            .flatten();
        Self {
            pane_id,
            placement_id,
            image_content_id,
            image_record,
            target_area,
            source_rect,
            cell_pixel_offset_x,
            cell_pixel_offset_y,
            z_index,
            draw_order: 0,
        }
    }
}

/// Return clipped image paints, bottom to top: sorted by z-index, then by the
/// record's image id and placement id (`0` when absent), then by draw order.
/// The draw order lists the images of every visible tiled pane, then those of
/// each floating pane this viewer draws, back to front. The viewer chrome of
/// `frame_geometry` decides which floating panes are drawn, as it does for
/// [`render_frame`](crate::render_frame).
///
/// An image is anchored at its pane's content area and cut to the cells of it
/// that are drawn: a tiled pane's content rect, or a floating pane's content
/// rect inside the pane area, each cut to the viewport area. An image whose
/// cut rectangle meets the outer rect of a floating pane drawn above its pane,
/// the viewer's open floating pane tray, its open notification center, or the
/// drawn cells of its toast is left out.
///
/// A placement at `(row: 1, column: 2)` with `column_count: 4` and `row_count: 3` in a
/// pane whose content starts at `(10, 5)` targets `(12, 6)` through
/// `(15, 8)`. If the pane ends at column 14, the target becomes two columns
/// wide and the source rectangle is cropped to the matching left half.
#[must_use]
pub fn build_image_paints(frame_geometry: &FrameGeometry<'_>) -> Vec<ImagePaint> {
    let render_snapshot = frame_geometry.render_snapshot;
    let screen_area = frame_geometry.viewport_area;
    if screen_area.width == 0
        || screen_area.height == 0
        || render_snapshot
            .session_snapshot
            .active_tab_snapshot
            .is_every_pane_suppressed
    {
        return Vec::new();
    }

    let placed_floating_panes = &frame_geometry.placed_floating_panes;
    let mut image_paints = Vec::new();

    for (pane_snapshot, placed_content_rect) in
        list_visible_tiled_panes(render_snapshot, frame_geometry.get_layout_origin())
    {
        push_pane_image_paints(
            pane_snapshot,
            placed_content_rect,
            placed_content_rect,
            placed_floating_panes,
            screen_area,
            &mut image_paints,
        );
    }
    for (floating_pane_index, placed_floating_pane) in placed_floating_panes.iter().enumerate() {
        let Some(pane_snapshot) =
            render_snapshot.find_pane_snapshot(placed_floating_pane.floating_pane_slot.pane_id)
        else {
            continue;
        };
        push_pane_image_paints(
            pane_snapshot,
            placed_floating_pane.content_area,
            placed_floating_pane.visible_content_area,
            &placed_floating_panes[floating_pane_index + 1..],
            screen_area,
            &mut image_paints,
        );
    }

    for covering_overlay_area in frame_geometry.list_overlay_areas() {
        image_paints.retain(|image_paint| {
            covering_overlay_area
                .intersection(image_paint.target_area)
                .is_empty()
        });
    }
    image_paints.sort_by_key(|image_paint| {
        (
            image_paint.z_index,
            image_paint.image_record.display.image_id.unwrap_or(0),
            image_paint.image_record.display.placement_id.unwrap_or(0),
            image_paint.draw_order,
        )
    });
    image_paints
}

/// Append one [`ImagePaint`] to `image_paints` for each image of
/// `pane_snapshot` that has a record and shows at least one cell
/// ([`list_drawn_image_placements`]), unless a floating pane in
/// `covering_floating_panes` covers it ([`is_covered_by_floating_pane`]). Each
/// paint's draw order is its index in `image_paints`.
fn push_pane_image_paints(
    pane_snapshot: &PaneSnapshot,
    content_area: RatatuiRect,
    clip_area: RatatuiRect,
    covering_floating_panes: &[PlacedFloatingPane],
    screen_area: RatatuiRect,
    image_paints: &mut Vec<ImagePaint>,
) {
    for (image_placement_snapshot, image_rect, target_area) in
        list_drawn_image_placements(pane_snapshot, content_area, clip_area, screen_area)
    {
        if is_covered_by_floating_pane(target_area, covering_floating_panes) {
            continue;
        }
        let Some(image_record) = image_placement_snapshot.clone_image_record() else {
            continue;
        };
        let Some(source_rect) = compute_image_source_rect(
            image_rect,
            target_area,
            image_placement_snapshot,
            &image_record,
        ) else {
            continue;
        };
        if source_rect.pixel_width == 0 || source_rect.pixel_height == 0 {
            continue;
        }
        let is_kitty_protocol = image_record.protocol == GraphicsProtocol::Kitty;
        let cell_pixel_offset_x = (is_kitty_protocol
            && target_area.x == image_rect.x
            && image_placement_snapshot
                .get_cell_geometry()
                .cell_offset
                .column
                == 0)
            .then_some(image_record.display.cell_pixel_offset_x)
            .flatten();
        let cell_pixel_offset_y = (is_kitty_protocol
            && target_area.y == image_rect.y
            && image_placement_snapshot.get_cell_geometry().cell_offset.row == 0)
            .then_some(image_record.display.cell_pixel_offset_y)
            .flatten();
        image_paints.push(ImagePaint {
            pane_id: pane_snapshot.pane_id,
            placement_id: image_placement_snapshot.get_placement_id(),
            image_content_id: image_placement_snapshot.get_image_content_id(),
            z_index: image_record.display.z_index,
            image_record,
            target_area,
            source_rect,
            cell_pixel_offset_x,
            cell_pixel_offset_y,
            draw_order: image_paints.len(),
        });
    }
}

/// Return the visible cell rectangles of the images of `frame_geometry`'s
/// tiled panes that show the unavailable-image text, in draw order.
///
/// In `Placeholder` mode every image placement shows it. In `Native` mode a
/// placement shows it while its image record is missing, or while
/// `unavailable_image_keys` names the placement, and never while a floating
/// pane that `frame_geometry` draws covers it.
/// A placement preview hides placeholders in its softened panes.
pub(crate) fn compute_image_placeholder_rects(
    frame_geometry: &FrameGeometry<'_>,
    image_mode: ImageRenderMode,
    unavailable_image_keys: &[ImagePlacementKey],
    placement_presentation: Option<&PanePlacementPresentation>,
) -> Vec<RatatuiRect> {
    let render_snapshot = frame_geometry.render_snapshot;
    let screen_area = frame_geometry.viewport_area;
    if screen_area.width == 0
        || screen_area.height == 0
        || render_snapshot
            .session_snapshot
            .active_tab_snapshot
            .is_every_pane_suppressed
    {
        return Vec::new();
    }

    let mut placeholder_rects = Vec::new();
    for (pane_snapshot, placed_content_rect) in
        list_visible_tiled_panes(render_snapshot, frame_geometry.get_layout_origin())
    {
        if placement_presentation.is_some_and(|placement_presentation| {
            placement_presentation.is_pane_affected(pane_snapshot.pane_id)
        }) {
            continue;
        }
        for (image_placement_snapshot, _, target_area) in list_drawn_image_placements(
            pane_snapshot,
            placed_content_rect,
            placed_content_rect,
            screen_area,
        ) {
            if should_show_image_placeholder(
                pane_snapshot.pane_id,
                image_placement_snapshot,
                target_area,
                &frame_geometry.placed_floating_panes,
                image_mode,
                unavailable_image_keys,
            ) {
                placeholder_rects.push(target_area);
            }
        }
    }
    placeholder_rects
}

/// Return the visible cell rectangles of one floating pane's images that show
/// the unavailable-image text, in draw order: the images of
/// `placed_floating_pane` cut to its visible content area and `screen_area`,
/// under the rules of [`compute_image_placeholder_rects`].
/// `covering_floating_panes` are the floating panes drawn above it.
pub(crate) fn compute_floating_pane_image_placeholder_rects(
    render_snapshot: &RenderSnapshot,
    placed_floating_pane: &PlacedFloatingPane,
    covering_floating_panes: &[PlacedFloatingPane],
    screen_area: RatatuiRect,
    image_mode: ImageRenderMode,
    unavailable_image_keys: &[ImagePlacementKey],
) -> Vec<RatatuiRect> {
    let Some(pane_snapshot) =
        render_snapshot.find_pane_snapshot(placed_floating_pane.floating_pane_slot.pane_id)
    else {
        return Vec::new();
    };
    list_drawn_image_placements(
        pane_snapshot,
        placed_floating_pane.content_area,
        placed_floating_pane.visible_content_area,
        screen_area,
    )
    .filter(|(image_placement_snapshot, _, target_area)| {
        should_show_image_placeholder(
            pane_snapshot.pane_id,
            image_placement_snapshot,
            *target_area,
            covering_floating_panes,
            image_mode,
            unavailable_image_keys,
        )
    })
    .map(|(_, _, target_area)| target_area)
    .collect()
}

/// Each image placement of `pane_snapshot` that shows at least one cell, as
/// `(placement, cell rect, drawn cells)`: the cell rect is anchored at
/// `content_area`'s top-left cell, and the drawn cells are that rect cut to
/// `clip_area` and `screen_area`. A pane with no terminal grid shows none.
fn list_drawn_image_placements(
    pane_snapshot: &PaneSnapshot,
    content_area: RatatuiRect,
    clip_area: RatatuiRect,
    screen_area: RatatuiRect,
) -> impl Iterator<Item = (&ImagePlacementSnapshot, RatatuiRect, RatatuiRect)> {
    let image_placement_snapshots: &[ImagePlacementSnapshot] =
        if pane_snapshot.terminal_grid_view.is_some() {
            &pane_snapshot.image_placement_snapshots
        } else {
            &[]
        };
    image_placement_snapshots
        .iter()
        .filter_map(move |image_placement_snapshot| {
            let image_rect = compute_image_placement_rect(content_area, image_placement_snapshot)?;
            let target_area = image_rect.intersection(clip_area).intersection(screen_area);
            (!target_area.is_empty()).then_some((image_placement_snapshot, image_rect, target_area))
        })
}

/// Whether the image placement `image_placement_snapshot` of the pane
/// `pane_id`, drawn over `target_area`, shows the unavailable-image text:
/// always in `Placeholder` mode. In `Native` mode, while its record is missing
/// or `unavailable_image_keys` names it, unless a floating pane in
/// `covering_floating_panes` covers `target_area`.
fn should_show_image_placeholder(
    pane_id: PaneId,
    image_placement_snapshot: &ImagePlacementSnapshot,
    target_area: RatatuiRect,
    covering_floating_panes: &[PlacedFloatingPane],
    image_mode: ImageRenderMode,
    unavailable_image_keys: &[ImagePlacementKey],
) -> bool {
    match image_mode {
        ImageRenderMode::Placeholder => true,
        ImageRenderMode::Native => {
            if is_covered_by_floating_pane(target_area, covering_floating_panes) {
                return false;
            }
            let has_image_record = image_placement_snapshot.get_image_record().is_some();
            let is_image_unavailable = unavailable_image_keys
                .contains(&(pane_id, image_placement_snapshot.get_placement_id()));
            !has_image_record || is_image_unavailable
        }
    }
}

/// Whether `target_area` shares a cell with the outer area of a floating pane
/// in `covering_floating_panes`.
fn is_covered_by_floating_pane(
    target_area: RatatuiRect,
    covering_floating_panes: &[PlacedFloatingPane],
) -> bool {
    covering_floating_panes.iter().any(|placed_floating_pane| {
        !placed_floating_pane
            .outer_area
            .intersection(target_area)
            .is_empty()
    })
}

/// Paint the unavailable-image text over each image rectangle in draw order.
pub fn draw_image_placeholders(placeholder_rects: &[RatatuiRect], screen_buffer: &mut Buffer) {
    for placeholder_rect in placeholder_rects {
        let target_area = placeholder_rect.intersection(screen_buffer.area);
        if target_area.width == 0 || target_area.height == 0 {
            continue;
        }
        clear_screen_rect(target_area, screen_buffer);
        let mut message_characters = TERMINAL_IMAGE_UNAVAILABLE.chars();
        'paint: for row_offset in 0..target_area.height {
            for column_offset in 0..target_area.width {
                let Some(message_character) = message_characters.next() else {
                    break 'paint;
                };
                screen_buffer[(target_area.x + column_offset, target_area.y + row_offset)]
                    .set_char(message_character);
            }
        }
    }
}

fn clear_screen_rect(target_area: RatatuiRect, screen_buffer: &mut Buffer) {
    for row_offset in 0..target_area.height {
        for column_offset in 0..target_area.width {
            screen_buffer[(target_area.x + column_offset, target_area.y + row_offset)]
                .set_char(' ')
                .set_style(Style::default());
        }
    }
}

/// Build a safe destination rect from a pane content rect and a placement.
fn compute_image_placement_rect(
    content_rect: RatatuiRect,
    image_placement_snapshot: &ImagePlacementSnapshot,
) -> Option<RatatuiRect> {
    let (anchor_row, anchor_column) = image_placement_snapshot.get_anchor_cell();
    let (row_cell_count, column_cell_count) = image_placement_snapshot.get_cell_dimensions();
    let screen_column = u32::from(content_rect.x).checked_add(u32::from(anchor_column))?;
    let screen_row = u32::from(content_rect.y).checked_add(u32::from(anchor_row))?;
    let maximum_coordinate = u32::from(u16::MAX) + 1;
    if screen_column >= maximum_coordinate || screen_row >= maximum_coordinate {
        return None;
    }
    let clipped_column_cell_count =
        u32::from(column_cell_count).min(maximum_coordinate - screen_column);
    let clipped_row_cell_count = u32::from(row_cell_count).min(maximum_coordinate - screen_row);
    (clipped_column_cell_count > 0 && clipped_row_cell_count > 0).then_some(RatatuiRect {
        x: u16::try_from(screen_column).ok()?,
        y: u16::try_from(screen_row).ok()?,
        width: u16::try_from(clipped_column_cell_count).ok()?,
        height: u16::try_from(clipped_row_cell_count).ok()?,
    })
}

/// Map the clipped destination back to the proportional source pixels.
fn compute_image_source_rect(
    image_rect: RatatuiRect,
    target_area: RatatuiRect,
    image_placement_snapshot: &ImagePlacementSnapshot,
    image_record: &ImageRecord,
) -> Option<ImageSourceRect> {
    let (
        source_pixel_origin_x,
        source_pixel_origin_y,
        image_source_pixel_width,
        image_source_pixel_height,
    ) = image_record.compute_source_rect().ok()?;
    let cell_geometry = image_placement_snapshot.get_cell_geometry();
    let source_left_cell_column = u32::from(target_area.x) - u32::from(image_rect.x)
        + u32::from(cell_geometry.cell_offset.column);
    let source_top_cell_row = u32::from(target_area.y) - u32::from(image_rect.y)
        + u32::from(cell_geometry.cell_offset.row);
    let source_right_cell_column = source_left_cell_column + u32::from(target_area.width);
    let source_bottom_cell_row = source_top_cell_row + u32::from(target_area.height);
    let source_row_cell_count = cell_geometry.full_size.row_count;
    let source_column_cell_count = cell_geometry.full_size.column_count;
    let (source_pixel_x_offset, clipped_source_pixel_width) = compute_source_pixel_span(
        source_left_cell_column,
        source_right_cell_column,
        u32::from(source_column_cell_count),
        image_source_pixel_width,
    );
    let (source_pixel_y_offset, clipped_source_pixel_height) = compute_source_pixel_span(
        source_top_cell_row,
        source_bottom_cell_row,
        u32::from(source_row_cell_count),
        image_source_pixel_height,
    );
    Some(ImageSourceRect {
        pixel_x: source_pixel_origin_x.checked_add(source_pixel_x_offset)?,
        pixel_y: source_pixel_origin_y.checked_add(source_pixel_y_offset)?,
        pixel_width: clipped_source_pixel_width,
        pixel_height: clipped_source_pixel_height,
    })
}

/// Map one half-open cell span to a half-open source-pixel span.
fn compute_source_pixel_span(
    source_start_cell_offset: u32,
    source_end_cell_offset: u32,
    source_cell_count: u32,
    source_pixel_count: u32,
) -> (u32, u32) {
    if source_cell_count == 0
        || source_pixel_count == 0
        || source_start_cell_offset >= source_end_cell_offset
    {
        return (0, 0);
    }
    let source_pixel_start = u64::from(source_start_cell_offset) * u64::from(source_pixel_count)
        / u64::from(source_cell_count);
    let source_pixel_end = (u64::from(source_end_cell_offset) * u64::from(source_pixel_count))
        .div_ceil(u64::from(source_cell_count));
    let source_pixel_start = u32::try_from(source_pixel_start.min(u64::from(source_pixel_count)))
        .unwrap_or(source_pixel_count);
    let source_pixel_end = u32::try_from(source_pixel_end.min(u64::from(source_pixel_count)))
        .unwrap_or(source_pixel_count);
    if source_pixel_end > source_pixel_start {
        (source_pixel_start, source_pixel_end - source_pixel_start)
    } else if source_pixel_start < source_pixel_count {
        (source_pixel_start, 1)
    } else {
        (source_pixel_start, 0)
    }
}

#[cfg(test)]
mod tests;
