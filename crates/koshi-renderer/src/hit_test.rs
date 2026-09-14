//! Mouse hit-testing: map a client-local screen cell to the UI region under it.
//!
//! A decoded mouse event carries a cell coordinate in the client's own screen
//! space (`(0, 0)` top-left, `column` rightward, `row` downward). Before koshi can act
//! on a click — focus a pane, drag a border, forward to a program — it must know
//! *what* that cell sits on. [`hit_test`] answers that from one frame's
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
//! - The rest is the **pane area**: the solved layout centered in the pane
//!   rectangle left by the committed region solve, with a dim letterbox margin
//!   around it when the client is larger than the size the layout was solved for.
//!   A click in that margin hits nothing.
//! - Inside the pane area, a pane's one-cell **border** ring is distinct from its
//!   **content**, and a collapsed stack member's title strip is its own region.
//!   A cell inside the pane area that no pane box covers hits nothing.

use koshi_core::geometry::{Direction, Point, Rect};
use koshi_core::ids::{PaneId, TabId};
use ratatui::layout::Rect as RatatuiRect;

use crate::render::{
    compute_content_rect, compute_pane_area, find_region_area, solve_tabline_layout,
};
use crate::snapshot::FrameLayout;

/// The UI region under a client-local screen cell, as classified by
/// [`hit_test`].
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
    },
    /// A collapsed stack member's title strip; clicking it activates that pane.
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
    /// scrolled tab strip; clicking it peeks toward the start.
    TablineScrollLeft {
        /// The first-visible tab index the click scrolls the strip to.
        target_tab_index: usize,
    },
    /// The right scroll arrow, shown when tabs are hidden off the right of a
    /// scrolled tab strip; clicking it peeks toward the end.
    TablineScrollRight {
        /// The first-visible tab index the click scrolls the strip to.
        target_tab_index: usize,
    },
    /// The tabline row, off any tab ribbon or arrow (session name, gap, or mode
    /// tag).
    Tabline,
    /// The keybinding statusline on the bottom row.
    Statusline,
    /// No region: the letterbox margin, a cell inside the pane area that no
    /// pane box covers, the too-small overlay, or a zero-size viewport.
    None,
}

/// Classify the client-local screen cell `at` against the frame `frame`.
///
/// Reads the frame in the renderer's own paint order so chrome wins over the
/// pane content beneath it: the committed tabline and statusline regions are
/// tested before the pane area, and the pane area is centered inside the
/// committed pane rectangle with a letterbox margin that hits nothing.
#[must_use]
pub fn hit_test(frame_layout: FrameLayout<'_>, screen_point: Point) -> HitRegion {
    let viewport_area = compute_viewport_area(frame_layout);
    if viewport_area.width == 0 || viewport_area.height == 0 {
        return HitRegion::None;
    }

    let active_tab_snapshot = &frame_layout.session_snapshot.active_tab_snapshot;
    // No room for any pane: the frame draws only the too-small overlay, no
    // chrome row and no pane.
    if active_tab_snapshot.are_all_panes_suppressed {
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

    // The pane area is the effective-sized layout centered in the rectangle
    // left by the committed regions. A cell outside it is letterbox margin.
    let effective_layout_rect = compute_content_rect(
        compute_frame_pane_area(frame_layout, viewport_area),
        active_tab_snapshot.effective_cell_size,
    );
    if !is_screen_point_inside(effective_layout_rect, screen_point) {
        return HitRegion::None;
    }
    // Shift into effective-layout space, where the slot and header rects live.
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
        if pane_slot.outer_rect.is_point_inside(layout_point) {
            return HitRegion::PaneBorder {
                pane_id: pane_slot.pane_id,
                side: get_border_side(pane_slot.outer_rect, layout_point),
            };
        }
    }

    HitRegion::None
}

/// Classify a cell on the tabline row at column `x`: a scroll arrow, the tab
/// whose ribbon spans it, or [`Tabline`](HitRegion::Tabline) off all of them.
fn classify_tabline_region(
    frame_layout: FrameLayout<'_>,
    tabline_rect: RatatuiRect,
    column: u16,
) -> HitRegion {
    let tabline_geometry = solve_tabline_layout(frame_layout.get_tabline_inputs(), tabline_rect);
    if let Some(left_scroll_arrow) = tabline_geometry.left_scroll_arrow {
        if column == left_scroll_arrow.start_column {
            return HitRegion::TablineScrollLeft {
                target_tab_index: left_scroll_arrow.target_first_visible_tab_index,
            };
        }
    }
    if let Some(right_scroll_arrow) = tabline_geometry.right_scroll_arrow {
        if column == right_scroll_arrow.start_column {
            return HitRegion::TablineScrollRight {
                target_tab_index: right_scroll_arrow.target_first_visible_tab_index,
            };
        }
    }
    for visible_tab_span in tabline_geometry.visible_tab_spans {
        if column >= visible_tab_span.start_column
            && column < visible_tab_span.start_column + visible_tab_span.column_count
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
/// border. It reads the frame the same way [`hit_test`] does: the layout
/// centered in the committed pane rectangle, with a letterbox margin around it.
#[must_use]
pub fn pane_content_rect(frame_layout: FrameLayout<'_>, pane_id: PaneId) -> Option<Rect> {
    let viewport_area = compute_viewport_area(frame_layout);
    if viewport_area.width == 0 || viewport_area.height == 0 {
        return None;
    }
    let active_tab_snapshot = &frame_layout.session_snapshot.active_tab_snapshot;
    if active_tab_snapshot.are_all_panes_suppressed {
        return None;
    }
    let effective_layout_rect = compute_content_rect(
        compute_frame_pane_area(frame_layout, viewport_area),
        active_tab_snapshot.effective_cell_size,
    );
    let pane_slot = active_tab_snapshot
        .pane_slots
        .iter()
        .find(|pane_slot| pane_slot.is_visible && pane_slot.pane_id == pane_id)?;
    let content_rect = pane_slot.content_rect?;
    Some(Rect::from_origin_and_size(
        Point {
            column: effective_layout_rect.x + content_rect.origin.column,
            row: effective_layout_rect.y + content_rect.origin.row,
        },
        content_rect.cell_size,
    ))
}

/// The 1-based cell inside `pane_id`'s content that client-local screen cell
/// `at` falls on, or [`None`] when `at` is outside that pane's content or the
/// pane is not drawn this frame.
///
/// A mouse report addresses the program's own grid, whose top-left content cell
/// is `(1, 1)`, so the caller forwards these coordinates straight into the pane.
#[must_use]
pub fn compute_pane_local_cell(
    frame_layout: FrameLayout<'_>,
    pane_id: PaneId,
    screen_point: Point,
) -> Option<(u16, u16)> {
    let content_rect = pane_content_rect(frame_layout, pane_id)?;
    if !content_rect.is_point_inside(screen_point) {
        return None;
    }
    Some((
        screen_point.column - content_rect.origin.column + 1,
        screen_point.row - content_rect.origin.row + 1,
    ))
}

/// The 0-based cell inside `pane_id`'s content that client-local screen cell
/// `at` falls on, with a cell outside that content pulled to the nearest edge.
/// [`None`] when the pane is not drawn this frame.
///
/// On a pane whose content spans columns 10–49, `at.column = 70` gives column
/// `39`, the pane's last, and `at.column = 3` gives column `0`, its first.
#[must_use]
pub fn compute_clamped_pane_cell(
    frame_layout: FrameLayout<'_>,
    pane_id: PaneId,
    screen_point: Point,
) -> Option<(u16, u16)> {
    let content_rect = pane_content_rect(frame_layout, pane_id)?;
    let right_column =
        content_rect.origin.column + content_rect.cell_size.column_count.saturating_sub(1);
    let bottom_row = content_rect.origin.row + content_rect.cell_size.row_count.saturating_sub(1);
    Some((
        screen_point
            .column
            .clamp(content_rect.origin.column, right_column)
            - content_rect.origin.column,
        screen_point.row.clamp(content_rect.origin.row, bottom_row) - content_rect.origin.row,
    ))
}

/// The metadata index of the first tab currently visible in `frame`'s committed
/// tabline window, or [`None`] when no tabline is drawn this frame — a zero-size
/// viewport, or every pane suppressed for want of room.
///
/// It resolves the same window the renderer draws and [`hit_test`] classifies.
#[must_use]
pub fn find_first_visible_tab_index(frame_layout: FrameLayout<'_>) -> Option<usize> {
    let viewport_area = compute_viewport_area(frame_layout);
    if viewport_area.width == 0 || viewport_area.height == 0 {
        return None;
    }
    if frame_layout
        .session_snapshot
        .active_tab_snapshot
        .are_all_panes_suppressed
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
fn compute_frame_pane_area(
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
fn compute_viewport_area(frame_layout: FrameLayout<'_>) -> RatatuiRect {
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

/// Whether `at` is inside the half-open ratatui rectangle `area`.
fn is_screen_point_inside(screen_rect: RatatuiRect, screen_point: Point) -> bool {
    screen_point.column >= screen_rect.x
        && screen_point.row >= screen_rect.y
        && u32::from(screen_point.column) < u32::from(screen_rect.x) + u32::from(screen_rect.width)
        && u32::from(screen_point.row) < u32::from(screen_rect.y) + u32::from(screen_rect.height)
}

/// The side of `rect`'s one-cell border ring that `point` lies on. `point` is
/// assumed to be within `rect` but not within its inner content area. A corner
/// cell resolves to its vertical side, so a border drag on a corner reads as the
/// left or right edge.
fn get_border_side(outer_rect: Rect, screen_point: Point) -> Direction {
    let right_column = outer_rect.origin.column + outer_rect.cell_size.column_count - 1;
    let bottom_row = outer_rect.origin.row + outer_rect.cell_size.row_count - 1;
    if screen_point.column == outer_rect.origin.column {
        Direction::Left
    } else if screen_point.column == right_column {
        Direction::Right
    } else if screen_point.row == outer_rect.origin.row {
        Direction::Up
    } else {
        debug_assert_eq!(
            screen_point.row, bottom_row,
            "border cell is on one of the four edges"
        );
        Direction::Down
    }
}

#[cfg(test)]
mod tests;
