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

use crate::render::{content_rect, find_pane, pane_area, place};
use crate::snapshot::{CommittedRegions, ImagePlacementSnapshot, RenderSnapshot};

/// The text a client paints when it cannot display terminal image pixels.
pub const TERMINAL_IMAGE_UNAVAILABLE: &str = "terminal image unavailable";

/// The largest shared cell snapshot used to classify image composition.
pub const MAX_IMAGE_CELL_SNAPSHOT_CELLS: usize = 262_144;

/// The identity of one image placement in a rendered pane.
pub type ImagePlacementKey = (PaneId, ImagePlacementId);

/// The cell facts needed to classify image composition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageCellState {
    /// The base character in the cell.
    pub ch: char,
    /// The terminal display width of the cell.
    pub width: u8,
    /// The combining and joined code points after the base character.
    pub combining: Vec<char>,
    /// The terminal style applied to the cell.
    pub style: CellStyle,
}

impl Default for ImageCellState {
    fn default() -> Self {
        Self {
            ch: ' ',
            width: 1,
            combining: Vec::new(),
            style: CellStyle::default(),
        }
    }
}

/// One bounded row-major snapshot of rendered cell facts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageCellSnapshot {
    /// The absolute frame area represented by `cells`.
    pub area: RatatuiRect,
    cells: Vec<ImageCellState>,
}

impl ImageCellSnapshot {
    /// Build a row-major cell snapshot when the area and cell count match.
    #[must_use]
    pub fn from_cells(area: RatatuiRect, cells: Vec<ImageCellState>) -> Option<Self> {
        let expected = usize::from(area.width).checked_mul(usize::from(area.height))?;
        if expected > MAX_IMAGE_CELL_SNAPSHOT_CELLS || cells.len() != expected {
            return None;
        }
        Some(Self { area, cells })
    }

    /// Return the cell at an absolute frame position.
    #[must_use]
    pub fn cell(&self, x: u16, y: u16) -> Option<&ImageCellState> {
        if x < self.area.x || y < self.area.y || x >= self.area.right() || y >= self.area.bottom() {
            return None;
        }
        let row = usize::from(y - self.area.y);
        let column = usize::from(x - self.area.x);
        let index = row
            .checked_mul(usize::from(self.area.width))?
            .checked_add(column)?;
        self.cells.get(index)
    }
}

/// Snapshot the rendered cell facts in one bounded frame area.
pub fn image_cell_snapshot(
    snapshot: &RenderSnapshot,
    committed_regions: &CommittedRegions,
    area: RatatuiRect,
) -> Option<ImageCellSnapshot> {
    let cells = usize::from(area.width).checked_mul(usize::from(area.height))?;
    if cells > MAX_IMAGE_CELL_SNAPSHOT_CELLS {
        return None;
    }
    let mut values = Vec::new();
    values.try_reserve_exact(cells).ok()?;
    values.resize(cells, ImageCellState::default());
    if cells == 0 {
        return Some(ImageCellSnapshot {
            area,
            cells: values,
        });
    }

    let content = content_rect(
        pane_area(committed_regions, area),
        snapshot.session.active_tab.effective_size,
    );
    let offset = koshi_core::geometry::Point {
        x: content.x,
        y: content.y,
    };
    for slot in &snapshot.session.active_tab.layout_solved {
        if !slot.visible {
            continue;
        }
        let Some(inner) = slot.inner_rect else {
            continue;
        };
        let Some(pane) = find_pane(snapshot, slot.pane_id) else {
            continue;
        };
        let Some(view) = &pane.grid_view else {
            continue;
        };
        let pane_area = place(inner, offset).intersection(area);
        for row in 0..pane_area.height {
            let y = pane_area.y + row;
            let grid_row = y.saturating_sub(place(inner, offset).y);
            for column in 0..pane_area.width {
                let x = pane_area.x + column;
                let grid_column = x.saturating_sub(place(inner, offset).x);
                let Some(cell) = view.grid.cell(grid_row, grid_column) else {
                    continue;
                };
                let index = usize::from(y - area.y)
                    .checked_mul(usize::from(area.width))?
                    .checked_add(usize::from(x - area.x))?;
                let selected = pane
                    .selection
                    .as_ref()
                    .and_then(|selection| selection.row_span(grid_row))
                    .is_some_and(|(start, end)| grid_column >= start && grid_column <= end);
                let mut style = cell.style();
                style.set_reverse(style.attrs().reverse() ^ pane.reverse_video ^ selected);
                values[index] = ImageCellState {
                    ch: cell.ch(),
                    width: cell.width(),
                    combining: cell.combining().to_vec(),
                    style,
                };
            }
        }
    }
    Some(ImageCellSnapshot {
        area,
        cells: values,
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
    pub x: u32,
    /// The source y coordinate in pixels.
    pub y: u32,
    /// The source width in pixels.
    pub width: u32,
    /// The source height in pixels.
    pub height: u32,
}

/// One image that can be painted inside a committed pane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImagePaint {
    /// The pane that owns the image.
    pub pane_id: PaneId,
    /// The terminal-local image placement identity.
    pub placement_id: ImagePlacementId,
    /// The connection-local image-record identity.
    pub content_id: u64,
    /// The complete image record, including row-major RGBA pixels.
    pub record: Arc<ImageRecord>,
    /// The destination cells after pane and frame clipping.
    pub target: RatatuiRect,
    /// The source pixels that map to `target`.
    pub source: ImageSourceRect,
    /// The Kitty x offset inside the first destination cell.
    pub cell_offset_x: Option<u32>,
    /// The Kitty y offset inside the first destination cell.
    pub cell_offset_y: Option<u32>,
    /// The protocol z-index used to order overlaps.
    pub z_index: i32,
    order: usize,
}

impl ImagePaint {
    /// Build one image paint from its clipped target and source rectangles.
    #[must_use]
    pub fn new(
        pane_id: PaneId,
        placement_id: ImagePlacementId,
        record: Arc<ImageRecord>,
        target: RatatuiRect,
        source: ImageSourceRect,
        z_index: i32,
    ) -> Self {
        let kitty = record.protocol == GraphicsProtocol::Kitty;
        let cell_offset_x = kitty.then_some(record.display.cell_offset_x).flatten();
        let cell_offset_y = kitty.then_some(record.display.cell_offset_y).flatten();
        Self {
            pane_id,
            placement_id,
            content_id: placement_id,
            record,
            target,
            source,
            cell_offset_x,
            cell_offset_y,
            z_index,
            order: 0,
        }
    }

    fn with_order(mut self, order: usize) -> Self {
        self.order = order;
        self
    }
}

/// Return clipped image paints in their bottom-to-top draw order.
///
/// A placement at `(row: 1, col: 2)` with `columns: 4` and `rows: 3` in a
/// pane whose content starts at `(10, 5)` targets `(12, 6)` through
/// `(15, 8)`. If the pane ends at column 14, the target becomes two columns
/// wide and the source rectangle is cropped to the matching left half.
#[must_use]
pub fn image_paints(
    snapshot: &RenderSnapshot,
    committed_regions: &CommittedRegions,
    area: RatatuiRect,
) -> Vec<ImagePaint> {
    if area.width == 0 || area.height == 0 || snapshot.session.active_tab.all_suppressed {
        return Vec::new();
    }

    let content = content_rect(
        pane_area(committed_regions, area),
        snapshot.session.active_tab.effective_size,
    );
    let offset = koshi_core::geometry::Point {
        x: content.x,
        y: content.y,
    };
    let mut paints = Vec::new();
    let mut order = 0;

    for slot in &snapshot.session.active_tab.layout_solved {
        if !slot.visible {
            continue;
        }
        let Some(inner) = slot.inner_rect else {
            continue;
        };
        let Some(pane) = find_pane(snapshot, slot.pane_id) else {
            continue;
        };
        if pane.grid_view.is_none() {
            continue;
        }
        let inner = place(inner, offset);
        for placement in &pane.image_placements {
            let Some(record) = placement.record_arc() else {
                continue;
            };
            let (rows, columns) = placement.dimensions();
            if columns == 0 || rows == 0 || record.image.width == 0 || record.image.height == 0 {
                continue;
            }
            let Some(image_rect) = placement_rect(inner, placement) else {
                continue;
            };
            let target = image_rect.intersection(inner).intersection(area);
            if target.width == 0 || target.height == 0 {
                continue;
            }
            let Some(source) = source_rect(image_rect, target, placement, &record) else {
                continue;
            };
            if source.width == 0 || source.height == 0 {
                continue;
            }
            let kitty = record.protocol == GraphicsProtocol::Kitty;
            let cell_offset_x =
                (kitty && target.x == image_rect.x && placement.geometry().offset.x == 0)
                    .then_some(record.display.cell_offset_x)
                    .flatten();
            let cell_offset_y =
                (kitty && target.y == image_rect.y && placement.geometry().offset.y == 0)
                    .then_some(record.display.cell_offset_y)
                    .flatten();
            let z_index = record.display.z_index;
            let mut paint =
                ImagePaint::new(pane.id, placement.id(), record, target, source, z_index)
                    .with_order(order);
            paint.content_id = placement.content_id();
            paint.cell_offset_x = cell_offset_x;
            paint.cell_offset_y = cell_offset_y;
            paints.push(paint);
            order = order.saturating_add(1);
        }
    }

    paints.sort_by_key(|paint| {
        (
            paint.z_index,
            paint.record.display.image_id.unwrap_or(0),
            paint.record.display.placement_id.unwrap_or(0),
            paint.order,
        )
    });
    paints
}

/// Return the visible cell rectangles of image placements.
///
/// With `only_unavailable`, a placement whose image record is present is
/// omitted. This lets a native-image viewer mark a missing transfer while an
/// unsupported viewer marks every image.
pub(crate) fn image_placeholder_rects(
    snapshot: &RenderSnapshot,
    committed_regions: &CommittedRegions,
    area: RatatuiRect,
    only_unavailable: bool,
) -> Vec<RatatuiRect> {
    if area.width == 0 || area.height == 0 || snapshot.session.active_tab.all_suppressed {
        return Vec::new();
    }

    let content = content_rect(
        pane_area(committed_regions, area),
        snapshot.session.active_tab.effective_size,
    );
    let offset = koshi_core::geometry::Point {
        x: content.x,
        y: content.y,
    };
    let mut rects = Vec::new();
    for slot in &snapshot.session.active_tab.layout_solved {
        if !slot.visible {
            continue;
        }
        let Some(inner) = slot.inner_rect else {
            continue;
        };
        let Some(pane) = find_pane(snapshot, slot.pane_id) else {
            continue;
        };
        if pane.grid_view.is_none() {
            continue;
        }
        let inner = place(inner, offset);
        for placement in &pane.image_placements {
            if only_unavailable && placement.record().is_some() {
                continue;
            }
            let Some(image_rect) = placement_rect(inner, placement) else {
                continue;
            };
            let target = image_rect.intersection(inner).intersection(area);
            if target.width > 0 && target.height > 0 {
                rects.push(target);
            }
        }
    }
    rects
}

/// Return image rectangles that still use the unavailable marker.
pub(crate) fn image_placeholder_rects_selected(
    snapshot: &RenderSnapshot,
    committed_regions: &CommittedRegions,
    area: RatatuiRect,
    available: Option<&[ImagePlacementKey]>,
) -> Vec<RatatuiRect> {
    if area.width == 0 || area.height == 0 || snapshot.session.active_tab.all_suppressed {
        return Vec::new();
    }

    let content = content_rect(
        pane_area(committed_regions, area),
        snapshot.session.active_tab.effective_size,
    );
    let offset = koshi_core::geometry::Point {
        x: content.x,
        y: content.y,
    };
    let mut rects = Vec::new();
    for slot in &snapshot.session.active_tab.layout_solved {
        if !slot.visible {
            continue;
        }
        let Some(inner) = slot.inner_rect else {
            continue;
        };
        let Some(pane) = find_pane(snapshot, slot.pane_id) else {
            continue;
        };
        if pane.grid_view.is_none() {
            continue;
        }
        let inner = place(inner, offset);
        for placement in &pane.image_placements {
            let selected = available.is_some_and(|keys| keys.contains(&(pane.id, placement.id())));
            if placement.record().is_some() && selected {
                continue;
            }
            let Some(image_rect) = placement_rect(inner, placement) else {
                continue;
            };
            let target = image_rect.intersection(inner).intersection(area);
            if target.width > 0 && target.height > 0 {
                rects.push(target);
            }
        }
    }
    rects
}

/// Paint unsupported-image text over each image rectangle in draw order.
pub(crate) fn draw_image_placeholders(rects: &[RatatuiRect], buf: &mut Buffer) {
    for rect in rects {
        let target = rect.intersection(buf.area);
        if target.width == 0 || target.height == 0 {
            continue;
        }
        clear_rect(target, buf);
        let mut message = TERMINAL_IMAGE_UNAVAILABLE.chars();
        'paint: for row in 0..target.height {
            for col in 0..target.width {
                let Some(ch) = message.next() else {
                    break 'paint;
                };
                buf[(target.x + col, target.y + row)].set_char(ch);
            }
        }
    }
}

fn clear_rect(target: RatatuiRect, buf: &mut Buffer) {
    for row in 0..target.height {
        for col in 0..target.width {
            buf[(target.x + col, target.y + row)]
                .set_char(' ')
                .set_style(Style::default());
        }
    }
}

/// Build a safe destination rect from a pane content rect and a placement.
fn placement_rect(inner: RatatuiRect, placement: &ImagePlacementSnapshot) -> Option<RatatuiRect> {
    let (anchor_row, anchor_column) = placement.anchor();
    let (rows, columns) = placement.dimensions();
    let x = u32::from(inner.x).checked_add(u32::from(anchor_column))?;
    let y = u32::from(inner.y).checked_add(u32::from(anchor_row))?;
    let max = u32::from(u16::MAX) + 1;
    if x >= max || y >= max {
        return None;
    }
    let width = u32::from(columns).min(max - x);
    let height = u32::from(rows).min(max - y);
    (width > 0 && height > 0).then_some(RatatuiRect {
        x: u16::try_from(x).ok()?,
        y: u16::try_from(y).ok()?,
        width: u16::try_from(width).ok()?,
        height: u16::try_from(height).ok()?,
    })
}

/// Map the clipped destination back to the proportional source pixels.
fn source_rect(
    image_rect: RatatuiRect,
    target: RatatuiRect,
    placement: &ImagePlacementSnapshot,
    record: &ImageRecord,
) -> Option<ImageSourceRect> {
    let (source_origin_x, source_origin_y, source_width, source_height) =
        record.source_rect().ok()?;
    let geometry = placement.geometry();
    let left = u32::from(target.x) - u32::from(image_rect.x) + u32::from(geometry.offset.x);
    let top = u32::from(target.y) - u32::from(image_rect.y) + u32::from(geometry.offset.y);
    let right = left + u32::from(target.width);
    let bottom = top + u32::from(target.height);
    let rows = geometry.full_size.rows;
    let columns = geometry.full_size.cols;
    let (x, width) = source_span(left, right, u32::from(columns), source_width);
    let (y, height) = source_span(top, bottom, u32::from(rows), source_height);
    Some(ImageSourceRect {
        x: source_origin_x.checked_add(x)?,
        y: source_origin_y.checked_add(y)?,
        width,
        height,
    })
}

/// Map one half-open cell span to a half-open source-pixel span.
fn source_span(start: u32, end: u32, cells: u32, pixels: u32) -> (u32, u32) {
    if cells == 0 || pixels == 0 || start >= end {
        return (0, 0);
    }
    let start = u64::from(start) * u64::from(pixels) / u64::from(cells);
    let end = (u64::from(end) * u64::from(pixels)).div_ceil(u64::from(cells));
    let start = u32::try_from(start.min(u64::from(pixels))).unwrap_or(pixels);
    let end = u32::try_from(end.min(u64::from(pixels))).unwrap_or(pixels);
    if end > start {
        (start, end - start)
    } else if start < pixels {
        (start, 1)
    } else {
        (start, 0)
    }
}

#[cfg(test)]
mod tests;
