//! Terminal-cell geometry.
//!
//! All coordinates and dimensions are measured in terminal **cells**, never
//! pixels. The origin `(0, 0)` is the top-left cell; `x` grows rightward
//! (columns) and `y` grows downward (rows).
//!
//! A [`Rect`] spans the half-open ranges `[x, x + cols)` × `[y, y + rows)`:
//! its right and bottom edges are exclusive. Zero-size rects are valid and
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

impl Size {
    /// The per-axis minimum of the two sizes: the smaller `cols` paired with
    /// the smaller `rows`. `40×10 min 20×24` → `20×10`.
    #[must_use]
    pub fn min_axes(self, other: Size) -> Size {
        Size {
            cols: self.cols.min(other.cols),
            rows: self.rows.min(other.rows),
        }
    }
}

/// The pane region a client reports for the tab it views.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PaneArea {
    /// The client draws the tab's panes inside a region of this size.
    Reported(Size),
    /// The client has no room to draw a pane.
    Starving,
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

impl Direction {
    /// The direction pointing the opposite way.
    #[must_use]
    pub fn opposite(self) -> Self {
        match self {
            Self::Left => Self::Right,
            Self::Right => Self::Left,
            Self::Up => Self::Down,
            Self::Down => Self::Up,
        }
    }
}

/// How a split divides space between its children.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SplitDirection {
    /// Left-right split.
    Horizontal,
    /// Top-bottom split.
    Vertical,
    /// The children overlay the same space instead of dividing it — a stack of
    /// panes with one visible at a time. There is no axis.
    Stacked,
}

impl Rect {
    /// Construct a rect from an origin and size.
    #[must_use]
    pub fn new(origin: Point, size: Size) -> Self {
        Self { origin, size }
    }

    /// The rect of the given size anchored at the origin `(0, 0)`.
    #[must_use]
    pub fn at_origin(size: Size) -> Self {
        Self {
            origin: Point { x: 0, y: 0 },
            size,
        }
    }

    /// The empty rect at the origin `(0, 0)` with zero size.
    #[must_use]
    pub fn zero() -> Self {
        Self::at_origin(Size { cols: 0, rows: 0 })
    }

    /// `true` when the rect covers no cells (zero width or zero height).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.size.cols == 0 || self.size.rows == 0
    }

    /// Exclusive right edge, `x + cols`, computed in `u32`.
    #[must_use]
    fn right(&self) -> u32 {
        u32::from(self.origin.x) + u32::from(self.size.cols)
    }

    /// Exclusive bottom edge, `y + rows`, computed in `u32`.
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

    /// The region of cells inside both `self` and `other`, or `None` when they
    /// share no cell.
    ///
    /// Each rect is half-open: `x` spans `[origin.x, origin.x + cols)` and `y`
    /// spans `[origin.y, origin.y + rows)`. Two rects that only touch at an
    /// edge or a corner share no cell. An empty rect shares no cell.
    ///
    /// ```text
    /// self:
    ///      *--------------------*
    ///      |        overlap     |
    ///      |        *-----------|----*
    ///      |        |###########|    |
    ///      *--------|-----------*    |
    ///               *----------------*
    ///                        other
    /// ```
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
    /// in by `border_cells` (saturating at `u16::MAX`) and each dimension loses
    /// `2 * border_cells` (saturating at `0`). Never panics.
    /// Origin `(2, 2)` size `10×8`, `inset(1)` → origin `(3, 3)` size `8×6`.
    #[must_use]
    fn inset(&self, border_cells: u16) -> Rect {
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

    /// The content area inside a one-cell border: `inset(1)`.
    #[must_use]
    pub fn inner_with_border(&self) -> Rect {
        self.inset(1)
    }
}

#[cfg(test)]
mod tests;
