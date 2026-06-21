//! Terminal-cell geometry.
//!
//! All coordinates and dimensions are measured in terminal **cells**, never
//! pixels. The origin `(0, 0)` is the top-left cell; `x` grows rightward
//! (columns) and `y` grows downward (rows).
//!
//! A [`Rect`] spans the half-open ranges `[x, x + cols)` × `[y, y + rows)`,
//! so its right and bottom edges are exclusive. Zero-size rects are valid and
//! representable (used for suppressed panes); every helper handles them and
//! the grid boundaries without panicking.

use serde::{Deserialize, Serialize};

/// A single cell coordinate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Point {
    /// Horizontal position (column).
    pub x: u16,
    /// Vertical position (row).
    pub y: u16,
}

/// A size in cells.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Size {
    /// Width in cells (columns).
    pub cols: u16,
    /// Height in cells (rows).
    pub rows: u16,
}

/// A rectangular region of cells, anchored at `origin` with the given `size`.
///
/// ```text
///
/// origin = Point { x, y }
///      ↓
///      *--------- cols ----------+
///      |                         |
///     rows                       |
///      |                         |
///      +-------------------------+
/// ```
///
/// `origin` is the top-left cell of the rectangle.
/// `size.cols` is the width, and `size.rows` is the height.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Rect {
    /// Top-left cell position.
    pub origin: Point,
    /// Width and height in cells.
    pub size: Size,
}

/// The two layout axes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Axis {
    /// X-axis (left-right).
    Horizontal,
    /// Y-axis (top-bottom).
    Vertical,
}

/// A cardinal direction, e.g. for focus movement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Direction {
    /// Leftward (negative x).
    Left,
    /// Rightward (positive x).
    Right,
    /// Upward (negative y).
    Up,
    /// Downward (positive y).
    Down,
}

/// How a split divides space between its children.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SplitDirection {
    /// Left-right split.
    Horizontal,
    /// Top-bottom split.
    Vertical,
    /// Overlaid split (z-axis).
    Stacked,
}

impl Rect {
    /// Construct a rect from an origin and size.
    #[must_use]
    pub fn new(origin: Point, size: Size) -> Self {
        Self { origin, size }
    }

    /// The empty rect at the origin `(0, 0)` with zero size.
    #[must_use]
    pub fn zero() -> Self {
        Self {
            origin: Point { x: 0, y: 0 },
            size: Size { cols: 0, rows: 0 },
        }
    }

    /// `true` when the rect covers no cells (zero width or zero height).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.size.cols == 0 || self.size.rows == 0
    }

    /// Exclusive right edge (`x + cols`), widened to avoid `u16` overflow.
    #[must_use]
    fn right(&self) -> u32 {
        u32::from(self.origin.x) + u32::from(self.size.cols)
    }

    /// Exclusive bottom edge (`y + rows`), widened to avoid `u16` overflow.
    #[must_use]
    fn bottom(&self) -> u32 {
        u32::from(self.origin.y) + u32::from(self.size.rows)
    }

    /// `true` when `point` lies within the half-open rect. An empty rect
    /// contains nothing.
    #[must_use]
    pub fn contains(&self, point: Point) -> bool {
        point.x >= self.origin.x
            && point.y >= self.origin.y
            && u32::from(point.x) < self.right()
            && u32::from(point.y) < self.bottom()
    }

    /// `true` when the two rects share at least one cell. Rects that merely
    /// touch along an edge do not intersect.
    #[must_use]
    pub fn intersects(&self, other: Rect) -> bool {
        self.intersection(other).is_some()
    }

    /// Returns the overlapping region between `self` and `other`.
    ///
    /// Rectangles are treated as half-open cell regions:
    ///
    /// ```text
    /// x range: [origin.x, right())
    /// y range: [origin.y, bottom())
    /// ```
    ///
    /// So the right and bottom edges are exclusive. Two rectangles that only touch
    /// at an edge or corner do not overlap.
    ///
    /// ```text
    /// self:
    ///      x0
    ///      *--------------------*
    ///      |                    |
    ///      |        overlap     |
    ///      |        *-----------|----*
    ///      |        |###########|    |
    ///      *--------|-----------*    |
    ///               |                |
    ///               *----------------*
    ///                        other
    /// ```
    ///
    /// The intersection is formed from:
    ///
    /// ```text
    /// left   = max(self.left,   other.left)
    /// top    = max(self.top,    other.top)
    /// right  = min(self.right,  other.right)
    /// bottom = min(self.bottom, other.bottom)
    /// ```
    ///
    /// If `right > left` and `bottom > top`, the rectangles share cells.
    /// Otherwise, they are disjoint or merely adjacent.
    #[must_use]
    pub fn intersection(&self, other: Rect) -> Option<Rect> {
        let x0 = self.origin.x.max(other.origin.x);
        let y0 = self.origin.y.max(other.origin.y);
        let x1 = self.right().min(other.right());
        let y1 = self.bottom().min(other.bottom());

        if x1 > u32::from(x0) && y1 > u32::from(y0) {
            Some(Rect {
                origin: Point { x: x0, y: y0 },
                size: Size {
                    cols: (x1 - u32::from(x0)) as u16,
                    rows: (y1 - u32::from(y0)) as u16,
                },
            })
        } else {
            None
        }
    }

    /// Shrink the rect inward by `border_cells` on every side. The origin moves
    /// in (saturating) and each dimension loses `2 * border_cells`, clamping to
    /// zero so under-/overflow is impossible.
    #[must_use]
    pub fn inset(&self, border_cells: u16) -> Rect {
        let both = border_cells.saturating_mul(2);
        Rect {
            origin: Point {
                x: self.origin.x.saturating_add(border_cells),
                y: self.origin.y.saturating_add(border_cells),
            },
            size: Size {
                cols: self.size.cols.saturating_sub(both),
                rows: self.size.rows.saturating_sub(both),
            },
        }
    }

    /// The content area inside a one-cell border. Convenience for `inset(1)`.
    #[must_use]
    pub fn inner_with_border(&self) -> Rect {
        self.inset(1)
    }
}

#[cfg(test)]
mod tests;
