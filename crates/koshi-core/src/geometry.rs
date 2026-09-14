//! Terminal-cell geometry.
//!
//! Coordinates and layout sizes are measured in terminal cells. Pixel cell
//! measurements describe the conversion used by terminal image protocols.
//! The origin `(0, 0)` is the top-left cell; `column` grows rightward and
//! `row` grows downward.
//!
//! A [`Rect`] spans the half-open ranges
//! `[column, column + column_count)` × `[row, row + row_count)`:
//! its right and bottom edges are exclusive. Zero-size rects are valid and
//! representable (used for suppressed panes); every helper handles them and
//! the grid boundaries without panicking.

use serde::{Deserialize, Serialize};

/// A single cell coordinate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Point {
    /// Horizontal position (column).
    #[serde(rename = "x")]
    pub column: u16,
    /// Vertical position (row).
    #[serde(rename = "y")]
    pub row: u16,
}

/// A size in cells.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Size {
    /// Width in cells (columns).
    #[serde(rename = "cols")]
    pub column_count: u16,
    /// Height in cells (rows).
    #[serde(rename = "rows")]
    pub row_count: u16,
}

/// The complete image size in cells and the cells removed from its top and left.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ImageCellGeometry {
    /// The image's cell dimensions before clipping.
    pub full_size: Size,
    /// The clipped columns and rows measured from the complete image's origin.
    #[serde(rename = "offset")]
    pub cell_offset: Point,
}

impl ImageCellGeometry {
    /// Whether a visible rectangle of `size` fits inside the complete image.
    #[must_use]
    pub fn is_visible_size_contained(self, visible_size: Size) -> bool {
        visible_size.column_count > 0
            && visible_size.row_count > 0
            && u32::from(self.cell_offset.column) + u32::from(visible_size.column_count)
                <= u32::from(self.full_size.column_count)
            && u32::from(self.cell_offset.row) + u32::from(visible_size.row_count)
                <= u32::from(self.full_size.row_count)
    }
}

/// The measured width and height of one terminal cell in pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PixelCellSize {
    #[serde(rename = "width")]
    pixel_width: std::num::NonZeroU16,
    #[serde(rename = "height")]
    pixel_height: std::num::NonZeroU16,
}

impl PixelCellSize {
    /// Build a cell measurement when both pixel dimensions are nonzero.
    #[must_use]
    pub fn from_pixel_dimensions(pixel_width: u16, pixel_height: u16) -> Option<Self> {
        Some(Self {
            pixel_width: std::num::NonZeroU16::new(pixel_width)?,
            pixel_height: std::num::NonZeroU16::new(pixel_height)?,
        })
    }

    /// Return the width of one cell in pixels.
    #[must_use]
    pub fn get_pixel_width(self) -> u16 {
        self.pixel_width.get()
    }

    /// Return the height of one cell in pixels.
    #[must_use]
    pub fn get_pixel_height(self) -> u16 {
        self.pixel_height.get()
    }
}

impl Size {
    /// The per-axis minimum of the two sizes: the smaller column count paired
    /// with the smaller row count. `40×10 min 20×24` → `20×10`.
    #[must_use]
    pub fn compute_minimum_axes(self, other_size: Size) -> Size {
        Size {
            column_count: self.column_count.min(other_size.column_count),
            row_count: self.row_count.min(other_size.row_count),
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

/// A rectangular region of cells, anchored at `origin` with the given cell size.
///
/// ```text
///
/// origin = Point { column, row }
///      ↓
///      *------ column_count -----+
///      |                         |
///   row_count                    |
///      |                         |
///      +-------------------------+
/// ```
///
/// `origin` is the top-left cell of the rectangle.
/// `cell_size.column_count` is the width, and `cell_size.row_count` is the height.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Rect {
    /// Top-left cell position.
    pub origin: Point,
    /// Width and height in cells.
    #[serde(rename = "size")]
    pub cell_size: Size,
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
    pub fn compute_opposite_direction(self) -> Self {
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
    pub fn from_origin_and_size(origin: Point, cell_size: Size) -> Self {
        Self { origin, cell_size }
    }

    /// The rect of the given size anchored at the origin `(0, 0)`.
    #[must_use]
    pub fn from_size_at_origin(cell_size: Size) -> Self {
        Self {
            origin: Point { column: 0, row: 0 },
            cell_size,
        }
    }

    /// The empty rect at the origin `(0, 0)` with zero size.
    #[must_use]
    pub fn empty_at_origin() -> Self {
        Self::from_size_at_origin(Size {
            column_count: 0,
            row_count: 0,
        })
    }

    /// `true` when the rect covers no cells (zero width or zero height).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.cell_size.column_count == 0 || self.cell_size.row_count == 0
    }

    /// Exclusive right edge, `column + column_count`, computed in `u32`.
    #[must_use]
    fn get_right_edge(&self) -> u32 {
        u32::from(self.origin.column) + u32::from(self.cell_size.column_count)
    }

    /// Exclusive bottom edge, `row + row_count`, computed in `u32`.
    #[must_use]
    fn get_bottom_edge(&self) -> u32 {
        u32::from(self.origin.row) + u32::from(self.cell_size.row_count)
    }

    /// `true` when `point` lies within the half-open rect. An empty rect
    /// contains nothing.
    #[must_use]
    pub fn is_point_inside(&self, point: Point) -> bool {
        point.column >= self.origin.column
            && point.row >= self.origin.row
            && u32::from(point.column) < self.get_right_edge()
            && u32::from(point.row) < self.get_bottom_edge()
    }

    /// The region of cells inside both `self` and `other`, or `None` when they
    /// share no cell.
    ///
    /// Each rect is half-open: `column` spans `[origin.column, origin.column +
    /// column_count)` and `row` spans `[origin.row, origin.row + row_count)`.
    /// Two rects that only touch at an edge or a corner share no cell. An
    /// empty rect shares no cell.
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
    pub fn compute_intersection(&self, other_rect: Rect) -> Option<Rect> {
        let left_edge = self.origin.column.max(other_rect.origin.column);
        let top_edge = self.origin.row.max(other_rect.origin.row);
        let right_edge = self.get_right_edge().min(other_rect.get_right_edge());
        let bottom_edge = self.get_bottom_edge().min(other_rect.get_bottom_edge());

        if right_edge > u32::from(left_edge) && bottom_edge > u32::from(top_edge) {
            Some(Rect {
                origin: Point {
                    column: left_edge,
                    row: top_edge,
                },
                cell_size: Size {
                    column_count: (right_edge - u32::from(left_edge)) as u16,
                    row_count: (bottom_edge - u32::from(top_edge)) as u16,
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
    fn compute_inset(&self, border_cells: u16) -> Rect {
        let double_border_cells = border_cells.saturating_mul(2);
        Rect {
            origin: Point {
                column: self.origin.column.saturating_add(border_cells),
                row: self.origin.row.saturating_add(border_cells),
            },
            cell_size: Size {
                column_count: self
                    .cell_size
                    .column_count
                    .saturating_sub(double_border_cells),
                row_count: self.cell_size.row_count.saturating_sub(double_border_cells),
            },
        }
    }

    /// The content area inside a one-cell border: `inset(1)`.
    #[must_use]
    pub fn compute_inner_with_border(&self) -> Rect {
        self.compute_inset(1)
    }
}

#[cfg(test)]
mod tests;
