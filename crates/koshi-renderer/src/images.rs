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

use crate::render::{compute_content_rect, compute_pane_area, find_pane_snapshot, place_cell_rect};
use crate::snapshot::{CommittedRegions, ImagePlacementSnapshot, RenderSnapshot};

/// The text a client paints when it cannot display terminal image pixels.
pub const TERMINAL_IMAGE_UNAVAILABLE: &str = "terminal image unavailable";

/// The largest shared cell snapshot used to classify image composition.
pub const MAX_IMAGE_CELL_SNAPSHOT_CELL_COUNT: usize = 262_144;

/// The identity of one image placement in a rendered pane.
pub type ImagePlacementKey = (PaneId, ImagePlacementId);

/// The cell facts needed to classify image composition.
#[derive(Debug, Clone, PartialEq, Eq)]
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

/// Snapshot the rendered cell facts in one bounded frame area.
pub fn build_image_cell_snapshot(
    render_snapshot: &RenderSnapshot,
    committed_regions: &CommittedRegions,
    screen_area: RatatuiRect,
) -> Option<ImageCellSnapshot> {
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

    let effective_layout_rect = compute_content_rect(
        compute_pane_area(committed_regions, screen_area),
        render_snapshot
            .session_snapshot
            .active_tab_snapshot
            .effective_cell_size,
    );
    let layout_origin = koshi_core::geometry::Point {
        column: effective_layout_rect.x,
        row: effective_layout_rect.y,
    };
    for pane_slot in &render_snapshot
        .session_snapshot
        .active_tab_snapshot
        .pane_slots
    {
        if !pane_slot.is_visible {
            continue;
        }
        let Some(content_rect) = pane_slot.content_rect else {
            continue;
        };
        let Some(pane_snapshot) = find_pane_snapshot(render_snapshot, pane_slot.pane_id) else {
            continue;
        };
        let Some(grid_view) = &pane_snapshot.terminal_grid_view else {
            continue;
        };
        let screen_pane_area =
            place_cell_rect(content_rect, layout_origin).intersection(screen_area);
        let placed_content_rect = place_cell_rect(content_rect, layout_origin);
        for row_offset in 0..screen_pane_area.height {
            let screen_row = screen_pane_area.y + row_offset;
            let grid_row_index = screen_row.saturating_sub(placed_content_rect.y);
            for column_offset in 0..screen_pane_area.width {
                let screen_column = screen_pane_area.x + column_offset;
                let grid_column_index = screen_column.saturating_sub(placed_content_rect.x);
                let Some(cell) = grid_view.grid.get_cell(grid_row_index, grid_column_index) else {
                    continue;
                };
                let cell_index = usize::from(screen_row - screen_area.y)
                    .checked_mul(usize::from(screen_area.width))?
                    .checked_add(usize::from(screen_column - screen_area.x))?;
                let is_selected = pane_snapshot
                    .selection_spans
                    .as_ref()
                    .and_then(|selection_spans| selection_spans.find_row_span(grid_row_index))
                    .is_some_and(|(start_column, end_column)| {
                        grid_column_index >= start_column && grid_column_index <= end_column
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
    }
    Some(ImageCellSnapshot {
        screen_area,
        cell_states,
    })
}

/// The image output capability selected for one attached terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageRenderMode {
    /// Paint image rectangles with the unsupported-image text.
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
    /// The source pixels that map to `target`.
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
    /// Build one image paint from its clipped target and source rectangles.
    #[must_use]
    pub fn from_image_placement(
        pane_id: PaneId,
        placement_id: ImagePlacementId,
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
            image_content_id: placement_id,
            image_record,
            target_area,
            source_rect,
            cell_pixel_offset_x,
            cell_pixel_offset_y,
            z_index,
            draw_order: 0,
        }
    }

    fn with_draw_order(mut self, draw_order: usize) -> Self {
        self.draw_order = draw_order;
        self
    }
}

/// Return clipped image paints in their bottom-to-top draw order.
///
/// A placement at `(row: 1, column: 2)` with `column_count: 4` and `row_count: 3` in a
/// pane whose content starts at `(10, 5)` targets `(12, 6)` through
/// `(15, 8)`. If the pane ends at column 14, the target becomes two columns
/// wide and the source rectangle is cropped to the matching left half.
#[must_use]
pub fn build_image_paints(
    render_snapshot: &RenderSnapshot,
    committed_regions: &CommittedRegions,
    screen_area: RatatuiRect,
) -> Vec<ImagePaint> {
    if screen_area.width == 0
        || screen_area.height == 0
        || render_snapshot
            .session_snapshot
            .active_tab_snapshot
            .are_all_panes_suppressed
    {
        return Vec::new();
    }

    let effective_layout_rect = compute_content_rect(
        compute_pane_area(committed_regions, screen_area),
        render_snapshot
            .session_snapshot
            .active_tab_snapshot
            .effective_cell_size,
    );
    let layout_origin = koshi_core::geometry::Point {
        column: effective_layout_rect.x,
        row: effective_layout_rect.y,
    };
    let mut image_paints = Vec::new();
    let mut draw_order = 0;

    for pane_slot in &render_snapshot
        .session_snapshot
        .active_tab_snapshot
        .pane_slots
    {
        if !pane_slot.is_visible {
            continue;
        }
        let Some(content_rect) = pane_slot.content_rect else {
            continue;
        };
        let Some(pane_snapshot) = find_pane_snapshot(render_snapshot, pane_slot.pane_id) else {
            continue;
        };
        if pane_snapshot.terminal_grid_view.is_none() {
            continue;
        }
        let placed_content_rect = place_cell_rect(content_rect, layout_origin);
        for image_placement_snapshot in &pane_snapshot.image_placement_snapshots {
            let Some(image_record) = image_placement_snapshot.clone_image_record() else {
                continue;
            };
            let (row_count, column_count) = image_placement_snapshot.get_cell_dimensions();
            if column_count == 0
                || row_count == 0
                || image_record.image.pixel_width == 0
                || image_record.image.pixel_height == 0
            {
                continue;
            }
            let Some(image_rect) =
                compute_image_placement_rect(placed_content_rect, image_placement_snapshot)
            else {
                continue;
            };
            let target_area = image_rect
                .intersection(placed_content_rect)
                .intersection(screen_area);
            if target_area.width == 0 || target_area.height == 0 {
                continue;
            }
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
            let z_index = image_record.display.z_index;
            let mut image_paint = ImagePaint::from_image_placement(
                pane_snapshot.pane_id,
                image_placement_snapshot.get_placement_id(),
                image_record,
                target_area,
                source_rect,
                z_index,
            )
            .with_draw_order(draw_order);
            image_paint.image_content_id = image_placement_snapshot.get_image_content_id();
            image_paint.cell_pixel_offset_x = cell_pixel_offset_x;
            image_paint.cell_pixel_offset_y = cell_pixel_offset_y;
            image_paints.push(image_paint);
            draw_order = draw_order.saturating_add(1);
        }
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

/// Return the visible cell rectangles of image placements.
///
/// With `only_unavailable`, a placement whose image record is present is
/// omitted. This lets a native-image viewer mark a missing transfer while an
/// unsupported viewer marks every image.
pub(crate) fn compute_image_placeholder_rects(
    render_snapshot: &RenderSnapshot,
    committed_regions: &CommittedRegions,
    screen_area: RatatuiRect,
    is_unavailable_only: bool,
) -> Vec<RatatuiRect> {
    if screen_area.width == 0
        || screen_area.height == 0
        || render_snapshot
            .session_snapshot
            .active_tab_snapshot
            .are_all_panes_suppressed
    {
        return Vec::new();
    }

    let effective_layout_rect = compute_content_rect(
        compute_pane_area(committed_regions, screen_area),
        render_snapshot
            .session_snapshot
            .active_tab_snapshot
            .effective_cell_size,
    );
    let layout_origin = koshi_core::geometry::Point {
        column: effective_layout_rect.x,
        row: effective_layout_rect.y,
    };
    let mut placeholder_rects = Vec::new();
    for pane_slot in &render_snapshot
        .session_snapshot
        .active_tab_snapshot
        .pane_slots
    {
        if !pane_slot.is_visible {
            continue;
        }
        let Some(content_rect) = pane_slot.content_rect else {
            continue;
        };
        let Some(pane_snapshot) = find_pane_snapshot(render_snapshot, pane_slot.pane_id) else {
            continue;
        };
        if pane_snapshot.terminal_grid_view.is_none() {
            continue;
        }
        let placed_content_rect = place_cell_rect(content_rect, layout_origin);
        for image_placement_snapshot in &pane_snapshot.image_placement_snapshots {
            if is_unavailable_only && image_placement_snapshot.get_image_record().is_some() {
                continue;
            }
            let Some(image_rect) =
                compute_image_placement_rect(placed_content_rect, image_placement_snapshot)
            else {
                continue;
            };
            let target_area = image_rect
                .intersection(placed_content_rect)
                .intersection(screen_area);
            if target_area.width > 0 && target_area.height > 0 {
                placeholder_rects.push(target_area);
            }
        }
    }
    placeholder_rects
}

/// Return image rectangles that still use the unavailable marker.
pub(crate) fn compute_selected_image_placeholder_rects(
    render_snapshot: &RenderSnapshot,
    committed_regions: &CommittedRegions,
    screen_area: RatatuiRect,
    available_image_keys: Option<&[ImagePlacementKey]>,
) -> Vec<RatatuiRect> {
    if screen_area.width == 0
        || screen_area.height == 0
        || render_snapshot
            .session_snapshot
            .active_tab_snapshot
            .are_all_panes_suppressed
    {
        return Vec::new();
    }

    let effective_layout_rect = compute_content_rect(
        compute_pane_area(committed_regions, screen_area),
        render_snapshot
            .session_snapshot
            .active_tab_snapshot
            .effective_cell_size,
    );
    let layout_origin = koshi_core::geometry::Point {
        column: effective_layout_rect.x,
        row: effective_layout_rect.y,
    };
    let mut placeholder_rects = Vec::new();
    for pane_slot in &render_snapshot
        .session_snapshot
        .active_tab_snapshot
        .pane_slots
    {
        if !pane_slot.is_visible {
            continue;
        }
        let Some(content_rect) = pane_slot.content_rect else {
            continue;
        };
        let Some(pane_snapshot) = find_pane_snapshot(render_snapshot, pane_slot.pane_id) else {
            continue;
        };
        if pane_snapshot.terminal_grid_view.is_none() {
            continue;
        }
        let placed_content_rect = place_cell_rect(content_rect, layout_origin);
        for image_placement_snapshot in &pane_snapshot.image_placement_snapshots {
            let is_image_available = available_image_keys.is_some_and(|image_keys| {
                image_keys.contains(&(
                    pane_snapshot.pane_id,
                    image_placement_snapshot.get_placement_id(),
                ))
            });
            if image_placement_snapshot.get_image_record().is_some() && is_image_available {
                continue;
            }
            let Some(image_rect) =
                compute_image_placement_rect(placed_content_rect, image_placement_snapshot)
            else {
                continue;
            };
            let target_area = image_rect
                .intersection(placed_content_rect)
                .intersection(screen_area);
            if target_area.width > 0 && target_area.height > 0 {
                placeholder_rects.push(target_area);
            }
        }
    }
    placeholder_rects
}

/// Paint unsupported-image text over each image rectangle in draw order.
pub(crate) fn draw_image_placeholders(placeholder_rects: &[RatatuiRect], buffer: &mut Buffer) {
    for placeholder_rect in placeholder_rects {
        let target_area = placeholder_rect.intersection(buffer.area);
        if target_area.width == 0 || target_area.height == 0 {
            continue;
        }
        clear_screen_rect(target_area, buffer);
        let mut message_characters = TERMINAL_IMAGE_UNAVAILABLE.chars();
        'paint: for row_offset in 0..target_area.height {
            for column in 0..target_area.width {
                let Some(message_character) = message_characters.next() else {
                    break 'paint;
                };
                buffer[(target_area.x + column, target_area.y + row_offset)]
                    .set_char(message_character);
            }
        }
    }
}

fn clear_screen_rect(target_area: RatatuiRect, buffer: &mut Buffer) {
    for row_offset in 0..target_area.height {
        for column_offset in 0..target_area.width {
            buffer[(target_area.x + column_offset, target_area.y + row_offset)]
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
    let (row_count, column_count) = image_placement_snapshot.get_cell_dimensions();
    let screen_column = u32::from(content_rect.x).checked_add(u32::from(anchor_column))?;
    let screen_row = u32::from(content_rect.y).checked_add(u32::from(anchor_row))?;
    let maximum_coordinate = u32::from(u16::MAX) + 1;
    if screen_column >= maximum_coordinate || screen_row >= maximum_coordinate {
        return None;
    }
    let clipped_column_count = u32::from(column_count).min(maximum_coordinate - screen_column);
    let clipped_row_count = u32::from(row_count).min(maximum_coordinate - screen_row);
    (clipped_column_count > 0 && clipped_row_count > 0).then_some(RatatuiRect {
        x: u16::try_from(screen_column).ok()?,
        y: u16::try_from(screen_row).ok()?,
        width: u16::try_from(clipped_column_count).ok()?,
        height: u16::try_from(clipped_row_count).ok()?,
    })
}

/// Map the clipped destination back to the proportional source pixels.
fn compute_image_source_rect(
    image_rect: RatatuiRect,
    target_area: RatatuiRect,
    image_placement_snapshot: &ImagePlacementSnapshot,
    image_record: &ImageRecord,
) -> Option<ImageSourceRect> {
    let (source_origin_x, source_origin_y, source_pixel_width, source_pixel_height) =
        image_record.compute_source_rect().ok()?;
    let cell_geometry = image_placement_snapshot.get_cell_geometry();
    let source_left = u32::from(target_area.x) - u32::from(image_rect.x)
        + u32::from(cell_geometry.cell_offset.column);
    let source_top = u32::from(target_area.y) - u32::from(image_rect.y)
        + u32::from(cell_geometry.cell_offset.row);
    let source_right = source_left + u32::from(target_area.width);
    let source_bottom = source_top + u32::from(target_area.height);
    let row_count = cell_geometry.full_size.row_count;
    let column_count = cell_geometry.full_size.column_count;
    let (pixel_x_offset, pixel_width) = compute_source_span(
        source_left,
        source_right,
        u32::from(column_count),
        source_pixel_width,
    );
    let (pixel_y_offset, pixel_height) = compute_source_span(
        source_top,
        source_bottom,
        u32::from(row_count),
        source_pixel_height,
    );
    Some(ImageSourceRect {
        pixel_x: source_origin_x.checked_add(pixel_x_offset)?,
        pixel_y: source_origin_y.checked_add(pixel_y_offset)?,
        pixel_width,
        pixel_height,
    })
}

/// Map one half-open cell span to a half-open source-pixel span.
fn compute_source_span(
    source_start: u32,
    source_end: u32,
    cell_count: u32,
    pixel_count: u32,
) -> (u32, u32) {
    if cell_count == 0 || pixel_count == 0 || source_start >= source_end {
        return (0, 0);
    }
    let pixel_start = u64::from(source_start) * u64::from(pixel_count) / u64::from(cell_count);
    let pixel_end =
        (u64::from(source_end) * u64::from(pixel_count)).div_ceil(u64::from(cell_count));
    let pixel_start = u32::try_from(pixel_start.min(u64::from(pixel_count))).unwrap_or(pixel_count);
    let pixel_end = u32::try_from(pixel_end.min(u64::from(pixel_count))).unwrap_or(pixel_count);
    if pixel_end > pixel_start {
        (pixel_start, pixel_end - pixel_start)
    } else if pixel_start < pixel_count {
        (pixel_start, 1)
    } else {
        (pixel_start, 0)
    }
}

#[cfg(test)]
mod tests;
