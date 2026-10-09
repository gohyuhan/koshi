//! Terminal-cell geometry.
//!
//! Coordinates and layout sizes are measured in terminal cells. A floating
//! pane's desired size is measured per axis in cells or in a percent of that
//! axis. Pixel cell measurements describe the conversion used by terminal image
//! protocols.
//! The origin `(0, 0)` is the top-left cell; `column` grows rightward and
//! `row` grows downward.
//!
//! A [`Rect`] spans the half-open ranges
//! `[column, column + column_count)` × `[row, row + row_count)`:
//! its right and bottom edges are exclusive. Zero-size rects are valid and
//! representable (used for suppressed panes); every helper handles them and
//! the grid boundaries without panicking.

use std::fmt;

use serde::{Deserialize, Serialize};

/// A single cell coordinate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Point {
    /// Horizontal position (column).
    pub column: u16,
    /// Vertical position (row).
    pub row: u16,
}

/// A size in cells.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Size {
    /// Width in cells (columns).
    pub column_count: u16,
    /// Height in cells (rows).
    pub row_count: u16,
}

/// The complete image size in cells and the cells removed from its top and left.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ImageCellGeometry {
    /// The image's cell dimensions before clipping.
    pub full_size: Size,
    /// The clipped columns and rows measured from the complete image's origin.
    pub cell_offset: Point,
}

impl ImageCellGeometry {
    /// Whether a visible rectangle of `visible_size` fits inside the complete image.
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
    pixel_width: std::num::NonZeroU16,
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

    /// Whether `self` fits inside `container_size`: each axis of `self` is at
    /// most the same axis of `container_size`. `22×10` fits inside `22×10` and
    /// `80×22`, and does not fit inside `21×40` or `80×9`.
    #[must_use]
    pub fn can_fit_inside(self, container_size: Size) -> bool {
        self.column_count <= container_size.column_count
            && self.row_count <= container_size.row_count
    }
}

/// The cells a layout needs on each axis. An axis can be larger than
/// `u16::MAX`, the largest axis a [`Size`] holds: a `65534`-column pane
/// minimum plus a 2-column border needs `65536` columns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RequiredSize {
    /// Columns needed.
    pub column_count: u32,
    /// Rows needed.
    pub row_count: u32,
}

impl RequiredSize {
    /// The per-axis sum of the two sizes. `20×6 plus 2×4` → `22×10`, and
    /// `65534×6 plus 2×4` → `65536×10`.
    #[must_use]
    pub fn from_size_sum(first_size: Size, second_size: Size) -> RequiredSize {
        RequiredSize {
            column_count: u32::from(first_size.column_count) + u32::from(second_size.column_count),
            row_count: u32::from(first_size.row_count) + u32::from(second_size.row_count),
        }
    }

    /// `self` as a [`Size`] when each axis of `self` is at most the same axis
    /// of `container_size`, else `None`. `22×10` inside `80×22` →
    /// `Some(22×10)`, and `65536×10` inside `65535×22` → `None`.
    #[must_use]
    pub fn fit_inside(self, container_size: Size) -> Option<Size> {
        let fitted_size = Size {
            column_count: u16::try_from(self.column_count).ok()?,
            row_count: u16::try_from(self.row_count).ok()?,
        };
        fitted_size
            .can_fit_inside(container_size)
            .then_some(fitted_size)
    }

    /// Whether each axis of `self` is at most the same axis of
    /// `container_size`: [`fit_inside`](Self::fit_inside) gives a size.
    #[must_use]
    pub fn can_fit_inside(self, container_size: Size) -> bool {
        self.fit_inside(container_size).is_some()
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

/// One axis of a floating pane's desired size: a number of cells, or a whole
/// percent of that axis.
///
/// Encodes as `{"Cells":80}` or `{"Percent":60}`. Decoding refuses
/// `{"Cells":0}`, `{"Percent":0}` and a percent above `100`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum FloatingPaneDimension {
    /// A nonzero number of cells.
    Cells(std::num::NonZeroU16),
    /// A whole percent of the axis, from `1` to `100`.
    Percent(AxisPercent),
}

/// A whole percent of one axis, from `1` to `100`.
///
/// Decoding refuses a value outside `1..=100` with [`AxisPercentError`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "u8")]
pub struct AxisPercent(u8);

/// The value [`AxisPercent::try_from`] refused: `0`, or `101` to `255`.
///
/// Displays as `percent 101 is outside 1 to 100`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AxisPercentError {
    /// The refused value.
    pub percent: u8,
}

impl fmt::Display for AxisPercentError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "percent {} is outside 1 to 100", self.percent)
    }
}

impl std::error::Error for AxisPercentError {}

impl TryFrom<u8> for AxisPercent {
    type Error = AxisPercentError;

    /// Accepts a percent from `1` to `100`.
    ///
    /// # Errors
    /// Returns [`AxisPercentError`] for `0` and for `101` to `255`.
    fn try_from(percent: u8) -> Result<Self, Self::Error> {
        if !(1..=100).contains(&percent) {
            return Err(AxisPercentError { percent });
        }
        Ok(Self(percent))
    }
}

impl AxisPercent {
    /// The percent, from `1` to `100`.
    #[must_use]
    pub fn get_percent(self) -> u8 {
        self.0
    }
}

/// The cells that `percent` percent of an axis `axis_cell_count` cells long
/// takes, rounded down. A `percent` above `100` counts as `100`. The result is
/// at most `axis_cell_count`. `60` percent of `22` cells → `13`.
#[must_use]
pub fn compute_percent_cell_count(axis_cell_count: u16, percent: u8) -> u16 {
    (u32::from(axis_cell_count) * u32::from(percent.min(100)) / 100) as u16
}

/// The size a floating pane asks for: one [`FloatingPaneDimension`] per axis.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct FloatingPaneSize {
    /// The width: a number of columns, or a percent of the horizontal axis.
    pub width: FloatingPaneDimension,
    /// The height: a number of rows, or a percent of the vertical axis.
    pub height: FloatingPaneDimension,
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
/// `size.column_count` is the width, and `size.row_count` is the height.
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
    pub fn from_origin_and_size(origin: Point, size: Size) -> Self {
        Self { origin, size }
    }

    /// The rect of the given size anchored at the origin `(0, 0)`.
    #[must_use]
    pub fn from_size_at_origin(size: Size) -> Self {
        Self {
            origin: Point { column: 0, row: 0 },
            size,
        }
    }

    /// The empty rect at the origin `(0, 0)` with zero size.
    #[must_use]
    pub fn build_empty_at_origin() -> Self {
        Self::from_size_at_origin(Size {
            column_count: 0,
            row_count: 0,
        })
    }

    /// `true` when the rect covers no cells (zero width or zero height).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.size.column_count == 0 || self.size.row_count == 0
    }

    /// Exclusive right edge, `column + column_count`, computed in `u32`.
    #[must_use]
    fn get_right_edge(&self) -> u32 {
        u32::from(self.origin.column) + u32::from(self.size.column_count)
    }

    /// Exclusive bottom edge, `row + row_count`, computed in `u32`.
    #[must_use]
    fn get_bottom_edge(&self) -> u32 {
        u32::from(self.origin.row) + u32::from(self.size.row_count)
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

    /// The region of cells inside both `self` and `other_rect`, or `None` when they
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
                size: Size {
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
            size: Size {
                column_count: self.size.column_count.saturating_sub(double_border_cells),
                row_count: self.size.row_count.saturating_sub(double_border_cells),
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
