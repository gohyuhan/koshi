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
    pub extent: u16,
}

/// The rectangles produced for ordered edge regions and the pane rectangle
/// that remains after all regions are applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegionSolve {
    /// One region rectangle for every input geometry, in input order.
    pub regions: Vec<Rect>,
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
pub fn solve(viewport: Size, geometries: &[RegionGeometry]) -> RegionSolve {
    let mut remaining = Rect::at_origin(viewport);
    let mut regions = Vec::with_capacity(geometries.len());

    for geometry in geometries {
        let region = match geometry.edge {
            Edge::Top => {
                let extent = geometry.extent.min(remaining.size.rows);
                let region = Rect::new(
                    remaining.origin,
                    Size {
                        cols: remaining.size.cols,
                        rows: extent,
                    },
                );
                remaining.origin.y += extent;
                remaining.size.rows -= extent;
                region
            }
            Edge::Bottom => {
                let extent = geometry.extent.min(remaining.size.rows);
                let region = Rect::new(
                    Point {
                        x: remaining.origin.x,
                        y: remaining.origin.y + remaining.size.rows - extent,
                    },
                    Size {
                        cols: remaining.size.cols,
                        rows: extent,
                    },
                );
                remaining.size.rows -= extent;
                region
            }
            Edge::Left => {
                let extent = geometry.extent.min(remaining.size.cols);
                let region = Rect::new(
                    remaining.origin,
                    Size {
                        cols: extent,
                        rows: remaining.size.rows,
                    },
                );
                remaining.origin.x += extent;
                remaining.size.cols -= extent;
                region
            }
            Edge::Right => {
                let extent = geometry.extent.min(remaining.size.cols);
                let region = Rect::new(
                    Point {
                        x: remaining.origin.x + remaining.size.cols - extent,
                        y: remaining.origin.y,
                    },
                    Size {
                        cols: extent,
                        rows: remaining.size.rows,
                    },
                );
                remaining.size.cols -= extent;
                region
            }
        };
        regions.push(region);
    }

    RegionSolve {
        regions,
        pane_rect: remaining,
    }
}

#[cfg(test)]
mod tests;
