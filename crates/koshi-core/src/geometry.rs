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
            && Rect::from_origin_and_size(self.cell_offset, visible_size)
                .is_inside_rect(Rect::from_size_at_origin(self.full_size))
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
    /// The cells this size covers: `column_count` times `row_count`. `200x50`
    /// → `10000`, and `65535x65535` → `4294836225`.
    #[must_use]
    pub fn compute_cell_count(self) -> u32 {
        u32::from(self.column_count) * u32::from(self.row_count)
    }

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

    /// The top-left cell of `content_size`, cut to `self` on each axis,
    /// centered in an area of `self`, each offset rounding down. `80x22` in
    /// `100x30` → `(10, 4)`; `120x40` in `100x30` → `(0, 0)`.
    #[must_use]
    pub fn compute_centered_origin(self, content_size: Size) -> Point {
        Point {
            column: (self.column_count - content_size.column_count.min(self.column_count)) / 2,
            row: (self.row_count - content_size.row_count.min(self.row_count)) / 2,
        }
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

    /// Whether each axis of `self` is at most the same axis of
    /// `container_size`. `22×10` fits inside `80×22`, and `65536×10` does not
    /// fit inside `65535×22`.
    #[must_use]
    pub fn can_fit_inside(self, container_size: Size) -> bool {
        self.column_count <= u32::from(container_size.column_count)
            && self.row_count <= u32::from(container_size.row_count)
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

/// The size a new floating pane asks for when its command names none: 60% of
/// each axis.
pub const DEFAULT_FLOATING_PANE_SIZE: FloatingPaneSize = FloatingPaneSize {
    width: FloatingPaneDimension::Percent(AxisPercent(60)),
    height: FloatingPaneDimension::Percent(AxisPercent(60)),
};

/// Where one client draws one floating pane: the default placement, or a
/// stored top-left cell counted from that client's pane-area origin.
///
/// Encodes as `"Default"`, `{"Moved":{"column":20,"row":5}}` or
/// `{"Pinned":{"column":20,"row":5}}`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum FloatingPanePosition {
    /// The client stored no cell: the pane draws centered in the client's
    /// floating viewport, moved by its cascade step.
    #[default]
    Default,
    /// The pane's top-left cell, stored by a move, a resize, or the
    /// `top_left_cell` of the command that created the pane.
    Moved(Point),
    /// The pane's top-left cell, locked: a move and a resize of the pane's
    /// left or top edge are refused.
    Pinned(Point),
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

    /// `true` when no cell of this rect lies outside `outer_rect`: the origin
    /// is at or past `outer_rect`'s origin on both axes, and the right and
    /// bottom edges stop at or before `outer_rect`'s. An empty rect within
    /// those bounds is inside.
    ///
    /// Example: the rect at column `2`, row `1`, `3` by `2` cells, is inside
    /// the rect at `(0, 0)`, `5` by `3` cells; at column `3`, it is not.
    #[must_use]
    pub fn is_inside_rect(&self, outer_rect: Rect) -> bool {
        self.origin.column >= outer_rect.origin.column
            && self.origin.row >= outer_rect.origin.row
            && self.get_right_edge() <= outer_rect.get_right_edge()
            && self.get_bottom_edge() <= outer_rect.get_bottom_edge()
    }

    /// This rect moved left and up until it lies inside the area of
    /// `container_size` that starts at `(0, 0)`. On an axis where the rect is
    /// larger than the container, its origin is `0`. The size stays the same.
    ///
    /// Example: in an `80x22` container, a `20x10` rect at `(70, 2)` moves to
    /// `(60, 2)`, and a `90x10` rect at `(5, 2)` moves to `(0, 2)`.
    #[must_use]
    pub fn compute_moved_inside(self, container_size: Size) -> Rect {
        Rect {
            origin: Point {
                column: self.origin.column.min(
                    container_size
                        .column_count
                        .saturating_sub(self.size.column_count),
                ),
                row: self
                    .origin
                    .row
                    .min(container_size.row_count.saturating_sub(self.size.row_count)),
            },
            size: self.size,
        }
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

    /// The side of this rect whose edge cells lie nearest `point`: the fewest
    /// cells from `point` to the rect's first or last column, or its first or
    /// last row. A tie goes to `Up`, then `Down`, then `Left`, then `Right`.
    ///
    /// Example: in the rect at `(40, 0)`, `40x22` cells, `(75, 10)` gives
    /// `Right`, and `(60, 1)` gives `Up`.
    #[must_use]
    pub fn compute_nearest_edge_direction(&self, point: Point) -> Direction {
        let last_column = self
            .origin
            .column
            .saturating_add(self.size.column_count.saturating_sub(1));
        let last_row = self
            .origin
            .row
            .saturating_add(self.size.row_count.saturating_sub(1));
        let left_distance = point.column.saturating_sub(self.origin.column);
        let right_distance = last_column.saturating_sub(point.column);
        let top_distance = point.row.saturating_sub(self.origin.row);
        let bottom_distance = last_row.saturating_sub(point.row);
        let nearest_edge_distance = left_distance
            .min(right_distance)
            .min(top_distance)
            .min(bottom_distance);
        if nearest_edge_distance == top_distance {
            Direction::Up
        } else if nearest_edge_distance == bottom_distance {
            Direction::Down
        } else if nearest_edge_distance == left_distance {
            Direction::Left
        } else {
            Direction::Right
        }
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
