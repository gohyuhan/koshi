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

use crate::render::{content_rect, find_pane, pane_area, place};
use crate::snapshot::{CommittedRegions, ImagePlacementSnapshot, RenderSnapshot};

/// The text a client paints when it cannot display terminal image pixels.
pub const TERMINAL_IMAGE_UNAVAILABLE: &str = "terminal image unavailable";

/// The image output capability selected for one attached terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageRenderMode {
    /// Paint image rectangles with the unsupported-image text.
    Placeholder,
    /// Keep ordinary cells beneath a native Kitty protocol writer.
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

    for (pane_order, slot) in snapshot.session.active_tab.layout_solved.iter().enumerate() {
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
            let cell_offset_x = (kitty && target.x == image_rect.x)
                .then_some(record.display.cell_offset_x)
                .flatten();
            let cell_offset_y = (kitty && target.y == image_rect.y)
                .then_some(record.display.cell_offset_y)
                .flatten();
            let z_index = record.display.z_index;
            let mut paint =
                ImagePaint::new(pane.id, placement.id(), record, target, source, z_index)
                    .with_order(
                        pane_order
                            .saturating_mul(placement_order_limit())
                            .saturating_add(order),
                    );
            paint.content_id = placement.content_id();
            paint.cell_offset_x = cell_offset_x;
            paint.cell_offset_y = cell_offset_y;
            paints.push(paint);
            order = order.saturating_add(1);
        }
    }

    paints.sort_by_key(|paint| (paint.z_index, paint.order));
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
    let left = u32::from(target.x) - u32::from(image_rect.x);
    let top = u32::from(target.y) - u32::from(image_rect.y);
    let right = left + u32::from(target.width);
    let bottom = top + u32::from(target.height);
    let (rows, columns) = placement.dimensions();
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

/// Keep the pane-order component separate from the placement sequence.
fn placement_order_limit() -> usize {
    usize::from(u16::MAX) + 1
}

#[cfg(test)]
mod tests;
