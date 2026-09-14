//! Pure geometry for ordered regions anchored to the four viewport edges.
//!
//! Each region is taken from the remaining pane rectangle. Nonzero regions
//! remove cells; zero-size regions leave it unchanged. The output keeps one
//! rectangle for every input geometry.

use koshi_core::geometry::{Point, Rect, Size};

/// The viewport edge that owns a region.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Edge {
    /// The top edge. The extent is measured in rows.
    Top,
    /// The bottom edge. The extent is measured in rows.
    Bottom,
    /// The left edge. The extent is measured in columns.
    Left,
    /// The right edge. The extent is measured in columns.
    Right,
}

/// The edge and extent of one region.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RegionGeometry {
    /// The viewport edge that owns the region.
    pub edge: Edge,
    /// The region's size along its edge axis, in cells.
    pub extent_cell_count: u16,
}

/// The rectangles produced for ordered edge regions and the pane rectangle
/// that remains after all regions are applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SolvedRegions {
    /// One region rectangle for every input geometry, in input order.
    pub region_rects: Vec<Rect>,
    /// The rectangle left for panes after all regions are applied.
    pub pane_rect: Rect,
}

/// Solve ordered edge regions inside a viewport.
///
/// Earlier nonzero regions are outermost and own each corner within their
/// extent. An extent larger than the remaining edge clamps to that edge length.
/// An extent of zero keeps a zero-size rectangle at its edge and removes no
/// cells.
///
/// For example, `80x24` with `Top 1` and `Bottom 1` produces region rectangles
/// at `(0, 0)` with size `80x1` and `(0, 23)` with size `80x1`; the pane
/// rectangle is `(0, 1)` with size `80x22`.
#[must_use]
pub fn solve_region_rects(viewport: Size, geometries: &[RegionGeometry]) -> SolvedRegions {
    let mut remaining_rect = Rect::from_size_at_origin(viewport);
    let mut region_rects = Vec::with_capacity(geometries.len());

    for region_geometry in geometries {
        let region_rect = match region_geometry.edge {
            Edge::Top => {
                let row_count = region_geometry
                    .extent_cell_count
                    .min(remaining_rect.cell_size.row_count);
                let region_rect = Rect::from_origin_and_size(
                    remaining_rect.origin,
                    Size {
                        column_count: remaining_rect.cell_size.column_count,
                        row_count,
                    },
                );
                remaining_rect.origin.row += row_count;
                remaining_rect.cell_size.row_count -= row_count;
                region_rect
            }
            Edge::Bottom => {
                let row_count = region_geometry
                    .extent_cell_count
                    .min(remaining_rect.cell_size.row_count);
                let region_rect = Rect::from_origin_and_size(
                    Point {
                        column: remaining_rect.origin.column,
                        row: remaining_rect.origin.row + remaining_rect.cell_size.row_count
                            - row_count,
                    },
                    Size {
                        column_count: remaining_rect.cell_size.column_count,
                        row_count,
                    },
                );
                remaining_rect.cell_size.row_count -= row_count;
                region_rect
            }
            Edge::Left => {
                let column_count = region_geometry
                    .extent_cell_count
                    .min(remaining_rect.cell_size.column_count);
                let region_rect = Rect::from_origin_and_size(
                    remaining_rect.origin,
                    Size {
                        column_count,
                        row_count: remaining_rect.cell_size.row_count,
                    },
                );
                remaining_rect.origin.column += column_count;
                remaining_rect.cell_size.column_count -= column_count;
                region_rect
            }
            Edge::Right => {
                let column_count = region_geometry
                    .extent_cell_count
                    .min(remaining_rect.cell_size.column_count);
                let region_rect = Rect::from_origin_and_size(
                    Point {
                        column: remaining_rect.origin.column
                            + remaining_rect.cell_size.column_count
                            - column_count,
                        row: remaining_rect.origin.row,
                    },
                    Size {
                        column_count,
                        row_count: remaining_rect.cell_size.row_count,
                    },
                );
                remaining_rect.cell_size.column_count -= column_count;
                region_rect
            }
        };
        region_rects.push(region_rect);
    }

    SolvedRegions {
        region_rects,
        pane_rect: remaining_rect,
    }
}

#[cfg(test)]
mod tests;
