//! Mouse hit-testing: map a client-local screen cell to the UI region under it.
//!
//! A decoded mouse event carries a cell coordinate in the client's own screen
//! space (`(0, 0)` top-left, `column` rightward, `row` downward). Before koshi can act
//! on a click — focus a pane, drag a border, forward to a program — it must know
//! *what* that cell sits on. [`resolve_hit_region`] answers that from one frame's
//! [`FrameLayout`] — including its committed region solve — returning a
//! [`HitRegion`] label.
//! It only classifies; it never changes state and never forwards anything.
//!
//! The frame is read the same way [`crate::render`] draws it, so the region a
//! click lands on is the region that was painted there:
//!
//! - The **tabline** (top row) and the **statusline** (bottom row) are koshi-owned
//!   chrome painted last, over whatever lies beneath, so a click on those rows
//!   is chrome, not the pane under it.
//! - **Floating panes** come next, the top of this viewer's draw order first.
//!   They live in cells of the pane rectangle left by the committed region
//!   solve, with no letterbox offset, so a float drawn in the letterbox margin
//!   is hit there. Inside a float, its content, its border ring, its titlebar
//!   row and the controls on it are separate regions.
//! - The rest is the **pane area**: the solved layout centered in that pane
//!   rectangle, with a dim letterbox margin around it when the client is larger
//!   than the size the layout was solved for. A click in that margin hits
//!   nothing.
//! - Inside the pane area, a pane's one-cell **border** ring is distinct from its
//!   **content**, and a collapsed stack member's title strip is its own region.
//!   A cell inside the pane area that no pane box covers hits nothing.

use koshi_core::geometry::{Direction, Point, Rect, Size};
use koshi_core::ids::{PaneId, TabId};
use ratatui::layout::Rect as RatatuiRect;

use crate::floating_pane_chrome::compute_floating_pane_titlebar;
use crate::render::{
    compute_content_rect, compute_pane_area, find_region_area, is_placement_handle_shown,
    solve_tabline_layout,
};
use crate::snapshot::{FloatingPaneSlot, FrameLayout};

/// The UI region under a client-local screen cell, as classified by
/// [`resolve_hit_region`].
///
/// Every variant names a region the renderer draws this frame.
/// [`None`](HitRegion::None) is a cell on none of them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HitRegion {
    /// A pane's content area (inside its border) — the cells the program draws.
    PaneContent {
        /// The pane whose content was hit.
        pane_id: PaneId,
    },
    /// A pane's one-cell border ring, and which side of it.
    PaneBorder {
        /// The pane whose border was hit.
        pane_id: PaneId,
        /// The side the cell lies on. A corner cell resolves to the vertical
        /// side ([`Left`](Direction::Left)/[`Right`](Direction::Right)).
        side: Direction,
        /// The horizontal side ([`Up`](Direction::Up)/[`Down`](Direction::Down))
        /// a corner cell also lies on. `None` on every other border cell.
        corner_horizontal_side: Option<Direction>,
    },
    /// The painted placement handle in a tiled pane's top border, or the grab
    /// handle `⣿⣿` in a floating pane's titlebar.
    PlacementHandle {
        /// The pane whose handle was hit.
        pane_id: PaneId,
    },
    /// A floating pane's titlebar or separator row, on a cell that holds no
    /// control.
    PaneTitlebar {
        /// The floating pane whose titlebar was hit.
        pane_id: PaneId,
    },
    /// A floating pane's pin toggle `[ ]` or `[⚪]`.
    PanePinToggle {
        /// The floating pane whose pin toggle was hit.
        pane_id: PaneId,
    },
    /// A floating pane's minimize button `–`.
    PaneMinimizeButton {
        /// The floating pane whose minimize button was hit.
        pane_id: PaneId,
    },
    /// A floating pane's close button `✕`.
    PaneCloseButton {
        /// The floating pane whose close button was hit.
        pane_id: PaneId,
    },
    /// A collapsed stack member's title strip; clicking it focuses that pane.
    StackHeader {
        /// The collapsed pane the strip represents.
        pane_id: PaneId,
    },
    /// A tab's ribbon in the tabline; clicking it selects that tab.
    Tab {
        /// The tab the ribbon represents.
        tab_id: TabId,
    },
    /// The left scroll arrow, shown when tabs are hidden off the left of a
    /// scrolled tab strip; clicking it steps the strip one tab toward the start.
    TablineScrollLeft,
    /// The right scroll arrow, shown when tabs are hidden off the right of a
    /// scrolled tab strip; clicking it steps the strip one tab toward the end.
    TablineScrollRight,
    /// The tabline row, off any tab ribbon or arrow (session name, gap, or mode
    /// tag).
    Tabline,
    /// The keybinding statusline on the bottom row.
    Statusline,
    /// No region: the letterbox margin, a cell inside the pane area that no
    /// pane box covers, the too-small overlay, or a zero-size viewport.
    None,
}

/// Classify the client-local screen cell `screen_point` against `frame_layout`.
///
/// Reads the frame in the renderer's own paint order so chrome wins over the
/// pane content beneath it: the committed tabline and statusline regions are
/// tested first, then this viewer's drawn floating panes from the top of the
/// draw order down, then the tiled layer. The tiled layout is centered inside
/// the committed pane rectangle with a letterbox margin that hits nothing.
#[must_use]
pub fn resolve_hit_region(frame_layout: FrameLayout<'_>, screen_point: Point) -> HitRegion {
    let viewport_area = compute_viewport_area(frame_layout);
    if viewport_area.width == 0 || viewport_area.height == 0 {
        return HitRegion::None;
    }

    let active_tab_snapshot = &frame_layout.session_snapshot.active_tab_snapshot;
    // No room for any pane: the frame draws only the too-small overlay, no
    // chrome row and no pane.
    if active_tab_snapshot.is_every_pane_suppressed {
        return HitRegion::None;
    }

    // The chrome rows are painted last, over the pane area beneath them: a cell
    // on those rows is chrome, whatever the layout put there.
    let tabline_rect = compute_tabline_area(frame_layout, viewport_area);
    if is_screen_point_inside(tabline_rect, screen_point) {
        return classify_tabline_region(frame_layout, tabline_rect, screen_point.column);
    }
    if let Some(statusline_rect) = compute_statusline_area(frame_layout, viewport_area) {
        if is_screen_point_inside(statusline_rect, screen_point) {
            return HitRegion::Statusline;
        }
    }

    // Floating panes draw over the tiled layer and its letterbox margin.
    let frame_pane_area = compute_frame_pane_area(frame_layout, viewport_area);
    if let Some(pane_area_point) = compute_pane_area_point(frame_pane_area, screen_point) {
        for floating_pane_slot in frame_layout.list_drawn_floating_pane_slots().rev() {
            if floating_pane_slot
                .outer_rect
                .is_point_inside(pane_area_point)
            {
                return classify_floating_pane_region(floating_pane_slot, pane_area_point);
            }
        }
    }

    // The tiled layout is the effective-sized layout centered in the rectangle
    // left by the committed regions. A cell outside it is letterbox margin.
    let effective_layout_rect = compute_content_rect(frame_pane_area, active_tab_snapshot.tab_size);
    if !is_screen_point_inside(effective_layout_rect, screen_point) {
        return HitRegion::None;
    }
    // Shift into tab-layout space, where the slot and header rects live.
    let layout_point = Point {
        column: screen_point.column - effective_layout_rect.x,
        row: screen_point.row - effective_layout_rect.y,
    };

    // Collapsed stack member strips are koshi-owned and hit-test like a border.
    for stack_header in &active_tab_snapshot.stack_headers {
        if stack_header.header_rect.is_point_inside(layout_point) {
            return HitRegion::StackHeader {
                pane_id: stack_header.pane_id,
            };
        }
    }

    // Visible pane boxes: the content area inside the border wins; the border
    // ring is everything in the outer box that is not content.
    for pane_slot in &active_tab_snapshot.pane_slots {
        if !pane_slot.is_visible {
            continue;
        }
        if let Some(content_rect) = pane_slot.content_rect {
            if content_rect.is_point_inside(layout_point) {
                return HitRegion::PaneContent {
                    pane_id: pane_slot.pane_id,
                };
            }
        }
        let Some((border_side, corner_horizontal_side)) =
            resolve_border_side(pane_slot.outer_rect, layout_point)
        else {
            continue;
        };
        if border_side == Direction::Up
            && is_placement_handle_shown(pane_slot.pane_id, frame_layout.viewer_chrome)
            && compute_tiled_pane_top_border(pane_slot.outer_rect)
                .placement_handle_rect
                .is_some_and(|handle_rect| handle_rect.is_point_inside(layout_point))
        {
            return HitRegion::PlacementHandle {
                pane_id: pane_slot.pane_id,
            };
        }
        return HitRegion::PaneBorder {
            pane_id: pane_slot.pane_id,
            side: border_side,
            corner_horizontal_side,
        };
    }

    HitRegion::None
}

/// Classify `pane_area_point`, a cell inside `floating_pane_slot`'s outer rect
/// counted from the pane-area origin. The content wins, then the titlebar parts
/// that [`compute_floating_pane_titlebar`] places, then the border ring. Every
/// other cell is [`PaneTitlebar`](HitRegion::PaneTitlebar): the titlebar cells
/// with no control and the separator row.
///
/// A `48x12` float at `(0, 0)`: `(2, 1)` is its grab handle, `(36, 1)` its pin
/// toggle, `(41, 1)` minimize, `(44, 1)` close, `(5, 1)` and `(5, 2)` its
/// titlebar, `(0, 1)` its left border, and `(5, 3)` its content.
fn classify_floating_pane_region(
    floating_pane_slot: FloatingPaneSlot,
    pane_area_point: Point,
) -> HitRegion {
    let pane_id = floating_pane_slot.pane_id;
    if floating_pane_slot
        .content_rect
        .is_point_inside(pane_area_point)
    {
        return HitRegion::PaneContent { pane_id };
    }
    let outer_rect = floating_pane_slot.outer_rect;
    let titlebar = compute_floating_pane_titlebar(outer_rect);
    let is_on_titlebar_part = |part_rect: Option<Rect>| {
        part_rect.is_some_and(|part_rect| part_rect.is_point_inside(pane_area_point))
    };
    if is_on_titlebar_part(titlebar.grab_handle_rect) {
        return HitRegion::PlacementHandle { pane_id };
    }
    if is_on_titlebar_part(titlebar.pin_toggle_rect) {
        return HitRegion::PanePinToggle { pane_id };
    }
    if is_on_titlebar_part(titlebar.minimize_button_rect) {
        return HitRegion::PaneMinimizeButton { pane_id };
    }
    if is_on_titlebar_part(titlebar.close_button_rect) {
        return HitRegion::PaneCloseButton { pane_id };
    }
    match resolve_border_side(outer_rect, pane_area_point) {
        Some((side, corner_horizontal_side)) => HitRegion::PaneBorder {
            pane_id,
            side,
            corner_horizontal_side,
        },
        None => HitRegion::PaneTitlebar { pane_id },
    }
}

/// `screen_point` counted from the top-left cell of `frame_pane_area`, or
/// `None` when it lies left of or above that cell. A pane area at `(0, 1)`
/// turns screen `(30, 14)` into `(30, 13)`, and screen `(30, 0)` into `None`.
fn compute_pane_area_point(frame_pane_area: RatatuiRect, screen_point: Point) -> Option<Point> {
    Some(Point {
        column: screen_point.column.checked_sub(frame_pane_area.x)?,
        row: screen_point.row.checked_sub(frame_pane_area.y)?,
    })
}

/// The column, counted from the outer rect's left edge, of the first
/// placement handle cell: the cell after the top-left corner.
const PLACEMENT_HANDLE_START_COLUMN: u16 = 1;

/// The cells the placement handle `⠿⠿⠿` takes.
const PLACEMENT_HANDLE_COLUMN_COUNT: u16 = 3;

/// The column, counted from the outer rect's left edge, where the pane label
/// starts: the cell after the last placement handle cell.
const TILED_PANE_TITLE_START_COLUMN: u16 =
    PLACEMENT_HANDLE_START_COLUMN + PLACEMENT_HANDLE_COLUMN_COUNT;

/// The cells at the right end of the top border that the pane label leaves
/// to the border: one `─` and the top-right corner.
const TILED_PANE_TITLE_END_COLUMN_OFFSET: u16 = 2;

/// The cells of the parts of a tiled pane's top border, each one row tall on
/// the top border row, in the coordinates of the outer rect they come from.
/// A part with no room is `None`, and its cells are plain border cells.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TiledPaneTopBorder {
    /// The placement handle `⠿⠿⠿`, three cells.
    pub placement_handle_rect: Option<Rect>,
    /// The cells the pane label ` <title> ` may take, at least one.
    pub title_rect: Option<Rect>,
}

/// The top border parts of a tiled pane whose outer rect is `outer_rect`.
///
/// The top border reads `┌⠿⠿⠿ <title> ─┐` while the handle shows and
/// `┌─── <title> ─┐` while it does not: the handle in columns 1 to 3, and
/// the pane label from column 4 to the column before the last `─`. The
/// handle needs 5 columns and the label 7. A pane 5 or 6 columns wide keeps
/// the handle and draws no label. A pane 4 columns wide or narrower has
/// neither.
///
/// A `20`-column outer rect at `(0, 0)` gives the handle at `(1, 0)` `3x1`
/// and the label at `(4, 0)` `14x1`.
#[must_use]
pub fn compute_tiled_pane_top_border(outer_rect: Rect) -> TiledPaneTopBorder {
    let outer_column_count = outer_rect.size.column_count;
    let build_top_border_rect = |start_column: u16, part_column_count: u16| {
        Rect::from_origin_and_size(
            Point {
                column: outer_rect.origin.column.saturating_add(start_column),
                row: outer_rect.origin.row,
            },
            Size {
                column_count: part_column_count,
                row_count: 1,
            },
        )
    };
    let has_placement_handle_room =
        outer_column_count > PLACEMENT_HANDLE_START_COLUMN + PLACEMENT_HANDLE_COLUMN_COUNT;
    let title_column_count = outer_column_count
        .saturating_sub(TILED_PANE_TITLE_START_COLUMN + TILED_PANE_TITLE_END_COLUMN_OFFSET);
    TiledPaneTopBorder {
        placement_handle_rect: has_placement_handle_room.then(|| {
            build_top_border_rect(PLACEMENT_HANDLE_START_COLUMN, PLACEMENT_HANDLE_COLUMN_COUNT)
        }),
        title_rect: (title_column_count > 0)
            .then(|| build_top_border_rect(TILED_PANE_TITLE_START_COLUMN, title_column_count)),
    }
}

/// Classify a cell on the tabline row at `column`: a scroll arrow, the tab
/// whose ribbon spans it, or [`Tabline`](HitRegion::Tabline) off all of them.
fn classify_tabline_region(
    frame_layout: FrameLayout<'_>,
    tabline_rect: RatatuiRect,
    screen_column: u16,
) -> HitRegion {
    let tabline_geometry = solve_tabline_layout(frame_layout.get_tabline_inputs(), tabline_rect);
    if tabline_geometry.left_scroll_arrow_column == Some(screen_column) {
        return HitRegion::TablineScrollLeft;
    }
    if tabline_geometry.right_scroll_arrow_column == Some(screen_column) {
        return HitRegion::TablineScrollRight;
    }
    for visible_tab_span in tabline_geometry.visible_tab_spans {
        if screen_column >= visible_tab_span.start_column
            && screen_column < visible_tab_span.start_column + visible_tab_span.column_count
        {
            return HitRegion::Tab {
                tab_id: frame_layout.session_snapshot.tabs_metadata
                    [visible_tab_span.tab_metadata_index]
                    .tab_id,
            };
        }
    }
    HitRegion::Tabline
}

/// The content area of `pane_id` in client-local screen coordinates, or [`None`]
/// when the pane is not drawn this frame.
///
/// This is the region a program's own grid maps onto — its cells inside the
/// border. It reads the frame the same way [`resolve_hit_region`] does: a
/// floating pane this viewer draws sits at its slot's content rect moved by the
/// pane-area origin, and a tiled pane in the layout centered in the committed
/// pane rectangle, with a letterbox margin around it.
#[must_use]
pub fn find_pane_content_rect(frame_layout: FrameLayout<'_>, pane_id: PaneId) -> Option<Rect> {
    let viewport_area = compute_viewport_area(frame_layout);
    if viewport_area.width == 0 || viewport_area.height == 0 {
        return None;
    }
    let active_tab_snapshot = &frame_layout.session_snapshot.active_tab_snapshot;
    if active_tab_snapshot.is_every_pane_suppressed {
        return None;
    }
    let frame_pane_area = compute_frame_pane_area(frame_layout, viewport_area);
    if let Some(floating_pane_slot) = frame_layout.find_drawn_floating_pane_slot(pane_id) {
        let content_rect = floating_pane_slot.content_rect;
        return Some(Rect::from_origin_and_size(
            Point {
                column: frame_pane_area.x.saturating_add(content_rect.origin.column),
                row: frame_pane_area.y.saturating_add(content_rect.origin.row),
            },
            content_rect.size,
        ));
    }
    let effective_layout_rect = compute_content_rect(frame_pane_area, active_tab_snapshot.tab_size);
    let content_rect = active_tab_snapshot
        .find_visible_pane_slot(pane_id)?
        .content_rect?;
    Some(Rect::from_origin_and_size(
        Point {
            column: effective_layout_rect.x + content_rect.origin.column,
            row: effective_layout_rect.y + content_rect.origin.row,
        },
        content_rect.size,
    ))
}

/// The 0-based cell inside `pane_id`'s content that client-local screen cell
/// `screen_point` falls on, `(0, 0)` at the content's top-left cell, with a
/// cell outside that content pulled to the nearest edge. [`None`] when the
/// pane is not drawn this frame.
///
/// On a pane whose content spans columns 10–49, `screen_point.column = 70`
/// gives column `39`, the pane's last, and `screen_point.column = 3` gives
/// column `0`, its first.
#[must_use]
pub fn compute_clamped_pane_cell(
    frame_layout: FrameLayout<'_>,
    pane_id: PaneId,
    screen_point: Point,
) -> Option<Point> {
    let content_rect = find_pane_content_rect(frame_layout, pane_id)?;
    let right_column =
        content_rect.origin.column + content_rect.size.column_count.saturating_sub(1);
    let bottom_row = content_rect.origin.row + content_rect.size.row_count.saturating_sub(1);
    Some(Point {
        column: screen_point
            .column
            .clamp(content_rect.origin.column, right_column)
            - content_rect.origin.column,
        row: screen_point.row.clamp(content_rect.origin.row, bottom_row) - content_rect.origin.row,
    })
}

/// The metadata index of the first tab currently visible in `frame_layout`'s committed
/// tabline window, or [`None`] when no tabline is drawn this frame — a zero-size
/// viewport, or every pane suppressed for want of room.
///
/// It resolves the same window the renderer draws and [`resolve_hit_region`] classifies.
#[must_use]
pub fn find_first_visible_tab_index(frame_layout: FrameLayout<'_>) -> Option<usize> {
    let viewport_area = compute_viewport_area(frame_layout);
    if viewport_area.width == 0 || viewport_area.height == 0 {
        return None;
    }
    if frame_layout
        .session_snapshot
        .active_tab_snapshot
        .is_every_pane_suppressed
    {
        return None;
    }
    let tabline_rect = compute_tabline_area(frame_layout, viewport_area);
    if tabline_rect.width == 0 || tabline_rect.height == 0 {
        return None;
    }
    Some(
        solve_tabline_layout(frame_layout.get_tabline_inputs(), tabline_rect)
            .first_visible_tab_index,
    )
}

/// Return the pane rectangle from the committed solve, or the whole area for
/// the server-side layout view that has no client commit.
pub(crate) fn compute_frame_pane_area(
    frame_layout: FrameLayout<'_>,
    viewport_area: RatatuiRect,
) -> RatatuiRect {
    frame_layout
        .committed_regions
        .map_or(viewport_area, |committed_regions| {
            compute_pane_area(committed_regions, viewport_area)
        })
}

/// The viewing client's whole viewport as a screen rect, origin `(0, 0)`. A
/// client 80 cells across and 24 rows tall gives `x: 0, y: 0, width: 80,
/// height: 24`.
pub(crate) fn compute_viewport_area(frame_layout: FrameLayout<'_>) -> RatatuiRect {
    let viewport_size = frame_layout.committed_regions.map_or(
        frame_layout.client_snapshot.viewport_size,
        |committed_regions| committed_regions.viewport_size,
    );
    RatatuiRect {
        x: 0,
        y: 0,
        width: viewport_size.column_count,
        height: viewport_size.row_count,
    }
}

/// The tabline rectangle, using the committed solve when this is a painted
/// client frame and the built-in top row for a server layout view.
fn compute_tabline_area(frame_layout: FrameLayout<'_>, viewport_area: RatatuiRect) -> RatatuiRect {
    match frame_layout.committed_regions {
        Some(committed_regions) => {
            find_region_area(committed_regions, 0, viewport_area).unwrap_or(RatatuiRect {
                x: viewport_area.x,
                y: viewport_area.y,
                width: 0,
                height: 0,
            })
        }
        None => RatatuiRect {
            x: viewport_area.x,
            y: viewport_area.y,
            width: viewport_area.width,
            height: viewport_area.height.min(1),
        },
    }
}

/// The statusline rectangle, or `None` when a one-row viewport has no bottom
/// row distinct from its tabline.
fn compute_statusline_area(
    frame_layout: FrameLayout<'_>,
    viewport_area: RatatuiRect,
) -> Option<RatatuiRect> {
    if viewport_area.height < 2 {
        return None;
    }
    match frame_layout.committed_regions {
        Some(committed_regions) => find_region_area(committed_regions, 1, viewport_area),
        None => Some(RatatuiRect {
            x: viewport_area.x,
            y: viewport_area.bottom() - 1,
            width: viewport_area.width,
            height: 1,
        }),
    }
}

/// Whether `screen_point` is inside the half-open ratatui rectangle `screen_rect`.
fn is_screen_point_inside(screen_rect: RatatuiRect, screen_point: Point) -> bool {
    screen_point.column >= screen_rect.x
        && screen_point.row >= screen_rect.y
        && u32::from(screen_point.column) < u32::from(screen_rect.x) + u32::from(screen_rect.width)
        && u32::from(screen_point.row) < u32::from(screen_rect.y) + u32::from(screen_rect.height)
}

/// The side of `outer_rect`'s one-cell border ring that `screen_point` lies on,
/// and the horizontal side a corner cell also lies on. `None` when
/// `screen_point` is outside `outer_rect` or inside the ring. A corner cell
/// resolves to its vertical side and names its horizontal side. On a `10x5`
/// rect at `(0, 0)`: `(0, 0)` gives `Some((Left, Some(Up)))`, `(4, 0)` gives
/// `Some((Up, None))`, and `(4, 2)` gives `None`.
fn resolve_border_side(
    outer_rect: Rect,
    screen_point: Point,
) -> Option<(Direction, Option<Direction>)> {
    if !outer_rect.is_point_inside(screen_point) {
        return None;
    }
    let right_column = outer_rect.origin.column + outer_rect.size.column_count - 1;
    let bottom_row = outer_rect.origin.row + outer_rect.size.row_count - 1;
    let horizontal_side = if screen_point.row == outer_rect.origin.row {
        Some(Direction::Up)
    } else if screen_point.row == bottom_row {
        Some(Direction::Down)
    } else {
        None
    };
    if screen_point.column == outer_rect.origin.column {
        return Some((Direction::Left, horizontal_side));
    }
    if screen_point.column == right_column {
        return Some((Direction::Right, horizontal_side));
    }
    horizontal_side.map(|side| (side, None))
}

#[cfg(test)]
mod tests;
