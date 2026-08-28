//! Mouse hit-testing: map a client-local screen cell to the UI region under it.
//!
//! A decoded mouse event carries a cell coordinate in the client's own screen
//! space (`(0, 0)` top-left, `x` rightward, `y` downward). Before koshi can act
//! on a click — focus a pane, drag a border, forward to a program — it must know
//! *what* that cell sits on. [`hit_test`] answers that from one frame's
//! [`FrameLayout`] — including its committed region solve — returning a
//! [`HitRegion`] label.
//! It only classifies; it never changes state and never forwards anything.
//!
//! The frame is read the same way [`crate::render`] draws it, so the region a
//! click lands on is the region that was painted there:
//!
//! - The **tabline** (top row) and the **hint bar** (bottom row) are koshi-owned
//!   chrome painted last, over whatever lies beneath, so a click on those rows
//!   is chrome, not the pane under it.
//! - The rest is the **pane area**: the solved layout centered in the pane
//!   rectangle left by the committed region solve, with a dim letterbox margin
//!   around it when the client is larger than the size the layout was solved for.
//!   A click in that margin hits nothing.
//! - Inside the pane area, a pane's one-cell **border** ring is distinct from its
//!   **content**; a collapsed stack member's title strip hit-tests like a border.

use koshi_core::geometry::{Direction, Point, Rect};
use koshi_core::ids::{PaneId, TabId};
use ratatui::layout::Rect as RatatuiRect;

use crate::render::{content_rect, pane_area as committed_pane_area, region_area, tabline_layout};
use crate::snapshot::FrameLayout;

/// The UI region under a client-local screen cell, as classified by
/// [`hit_test`].
///
/// Every variant names a region the renderer actually draws this frame; the
/// caller decides what a click on each one does. [`None`](HitRegion::None) is
/// the letterbox margin, the too-small overlay, or a degenerate viewport:
/// nothing to act on.
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
        to: usize,
    },
    /// The right scroll arrow, shown when tabs are hidden off the right of a
    /// scrolled tab strip; clicking it peeks toward the end.
    TablineScrollRight {
        /// The first-visible tab index the click scrolls the strip to.
        to: usize,
    },
    /// The tabline row, off any tab ribbon or arrow (session name, gap, or mode
    /// tag).
    Tabline,
    /// The keybinding hint bar on the bottom row.
    Statusline,
    /// Nothing actionable: the letterbox margin, the too-small overlay, or a
    /// zero-size viewport.
    None,
}

/// Classify the client-local screen cell `at` against the frame `frame`.
///
/// Reads the frame in the renderer's own paint order so chrome wins over the
/// pane content beneath it: the committed tabline and hint-bar regions are
/// tested before the pane area, and the pane area is centered inside the
/// committed pane rectangle with a letterbox margin that hits nothing.
#[must_use]
pub fn hit_test(frame: FrameLayout<'_>, at: Point) -> HitRegion {
    let area = viewport_area(frame);
    if area.width == 0 || area.height == 0 {
        return HitRegion::None;
    }

    let tab = &frame.session.active_tab;
    // No room for any pane: the whole frame is the too-small overlay, and no
    // chrome or pane is drawn, so nothing is hit-testable.
    if tab.all_suppressed {
        return HitRegion::None;
    }

    // Chrome rows are painted last and cover the pane area beneath them, so a
    // click on those rows is chrome regardless of what the layout put there.
    let tabline = tabline_area(frame, area);
    if contains(tabline, at) {
        return tabline_region(frame, tabline, at.x);
    }
    if let Some(statusline) = statusline_area(frame, area) {
        if contains(statusline, at) {
            return HitRegion::Statusline;
        }
    }

    // The pane area is the effective-sized layout centered in the rectangle
    // left by the committed regions. A cell outside it is letterbox margin.
    let content = content_rect(frame_pane_area(frame, area), tab.effective_size);
    if at.x < content.x || at.x >= content.right() || at.y < content.y || at.y >= content.bottom() {
        return HitRegion::None;
    }
    // Shift into effective-layout space, where the slot and header rects live.
    let local = Point {
        x: at.x - content.x,
        y: at.y - content.y,
    };

    // Collapsed stack member strips are koshi-owned and hit-test like a border.
    for header in &tab.stack_headers {
        if header.rect.contains(local) {
            return HitRegion::StackHeader {
                pane_id: header.pane,
            };
        }
    }

    // Visible pane boxes: the content area inside the border wins; the border
    // ring is everything in the outer box that is not content.
    for slot in &tab.layout_solved {
        if !slot.visible {
            continue;
        }
        if let Some(inner) = slot.inner_rect {
            if inner.contains(local) {
                return HitRegion::PaneContent {
                    pane_id: slot.pane_id,
                };
            }
        }
        if slot.rect.contains(local) {
            return HitRegion::PaneBorder {
                pane_id: slot.pane_id,
                side: border_side(slot.rect, local),
            };
        }
    }

    HitRegion::None
}

/// Classify a cell on the tabline row at column `x`: a scroll arrow, the tab
/// whose ribbon spans it, or [`Tabline`](HitRegion::Tabline) off all of them.
fn tabline_region(frame: FrameLayout<'_>, area: RatatuiRect, x: u16) -> HitRegion {
    let layout = tabline_layout(frame, area);
    if let Some((arrow_x, to)) = layout.left_arrow {
        if x == arrow_x {
            return HitRegion::TablineScrollLeft { to };
        }
    }
    if let Some((arrow_x, to)) = layout.right_arrow {
        if x == arrow_x {
            return HitRegion::TablineScrollRight { to };
        }
    }
    for (meta_index, tab_x, width) in layout.tabs {
        if x >= tab_x && x < tab_x + width {
            return HitRegion::Tab {
                tab_id: frame.session.tabs_metadata[meta_index].id,
            };
        }
    }
    HitRegion::Tabline
}

/// The content area of `pane_id` in client-local screen coordinates, or [`None`]
/// when the pane is not drawn this frame.
///
/// This is the region a program's own grid maps onto — its cells inside the
/// border. Read the frame the same way [`hit_test`] does (the layout centered in
/// the committed pane rectangle with a letterbox margin), so a cell forwarded
/// to a program is the cell the user clicked.
#[must_use]
pub fn pane_content_rect(frame: FrameLayout<'_>, pane_id: PaneId) -> Option<Rect> {
    let area = viewport_area(frame);
    if area.width == 0 || area.height == 0 {
        return None;
    }
    let tab = &frame.session.active_tab;
    if tab.all_suppressed {
        return None;
    }
    let content = content_rect(frame_pane_area(frame, area), tab.effective_size);
    let slot = tab
        .layout_solved
        .iter()
        .find(|slot| slot.visible && slot.pane_id == pane_id)?;
    let inner = slot.inner_rect?;
    Some(Rect::new(
        Point {
            x: content.x + inner.origin.x,
            y: content.y + inner.origin.y,
        },
        inner.size,
    ))
}

/// The 1-based cell inside `pane_id`'s content that client-local screen cell
/// `at` falls on, or [`None`] when `at` is outside that pane's content or the
/// pane is not drawn this frame.
///
/// A mouse report addresses the program's own grid, whose top-left content cell
/// is `(1, 1)`, so the caller forwards these coordinates straight into the pane.
#[must_use]
pub fn pane_local_cell(frame: FrameLayout<'_>, pane_id: PaneId, at: Point) -> Option<(u16, u16)> {
    let rect = pane_content_rect(frame, pane_id)?;
    if !rect.contains(at) {
        return None;
    }
    Some((at.x - rect.origin.x + 1, at.y - rect.origin.y + 1))
}

/// The 0-based cell inside `pane_id`'s content that client-local screen cell
/// `at` falls on, with a cell outside that content pulled to the nearest edge.
/// [`None`] when the pane is not drawn this frame.
///
/// Clamping is what lets a gesture that wandered off the pane still name a cell
/// in it: on a pane whose content spans columns 10–49, `at.x = 70` gives column
/// `39`, the pane's last. Both the viewer resolving a highlight and the session
/// placing a mouse report read the cell this way, so one gesture names the same
/// cell to both.
#[must_use]
pub fn pane_cell_clamped(frame: FrameLayout<'_>, pane_id: PaneId, at: Point) -> Option<(u16, u16)> {
    let rect = pane_content_rect(frame, pane_id)?;
    let right = rect.origin.x + rect.size.cols.saturating_sub(1);
    let bottom = rect.origin.y + rect.size.rows.saturating_sub(1);
    Some((
        at.x.clamp(rect.origin.x, right) - rect.origin.x,
        at.y.clamp(rect.origin.y, bottom) - rect.origin.y,
    ))
}

/// The metadata index of the first tab currently visible in `frame`'s committed
/// tabline window, or [`None`] when no tabline is drawn this frame — a zero-size
/// viewport, or every pane suppressed for want of room.
///
/// The mouse-routing layer reads this to anchor a peek-drag and to step the
/// window on a wheel scroll. It resolves the same window the renderer draws and
/// [`hit_test`] classifies.
#[must_use]
pub fn tabline_first_visible(frame: FrameLayout<'_>) -> Option<usize> {
    let area = viewport_area(frame);
    if area.width == 0 || area.height == 0 {
        return None;
    }
    if frame.session.active_tab.all_suppressed {
        return None;
    }
    let tabline = tabline_area(frame, area);
    if tabline.width == 0 || tabline.height == 0 {
        return None;
    }
    Some(tabline_layout(frame, tabline).first_visible)
}

/// Return the pane rectangle from the committed solve, or the whole area for
/// the server-side layout view that has no client commit.
fn frame_pane_area(frame: FrameLayout<'_>, area: RatatuiRect) -> RatatuiRect {
    frame
        .committed_regions
        .map_or(area, |regions| committed_pane_area(regions, area))
}

/// The viewing client's whole viewport as a screen rect, origin `(0, 0)`. A
/// client 80 cells across and 24 rows tall gives `x: 0, y: 0, width: 80,
/// height: 24`.
fn viewport_area(frame: FrameLayout<'_>) -> RatatuiRect {
    let size = frame
        .committed_regions
        .map_or(frame.client.viewport, |regions| regions.viewport);
    RatatuiRect {
        x: 0,
        y: 0,
        width: size.cols,
        height: size.rows,
    }
}

/// The tabline rectangle, using the committed solve when this is a painted
/// client frame and the built-in top row for a server layout view.
fn tabline_area(frame: FrameLayout<'_>, area: RatatuiRect) -> RatatuiRect {
    match frame.committed_regions {
        Some(regions) => region_area(regions, 0, area).unwrap_or(RatatuiRect {
            x: area.x,
            y: area.y,
            width: 0,
            height: 0,
        }),
        None => RatatuiRect {
            x: area.x,
            y: area.y,
            width: area.width,
            height: area.height.min(1),
        },
    }
}

/// The statusline rectangle, or `None` when a one-row viewport has no bottom
/// row distinct from its tabline.
fn statusline_area(frame: FrameLayout<'_>, area: RatatuiRect) -> Option<RatatuiRect> {
    if area.height < 2 {
        return None;
    }
    match frame.committed_regions {
        Some(regions) => region_area(regions, 1, area),
        None => Some(RatatuiRect {
            x: area.x,
            y: area.bottom() - 1,
            width: area.width,
            height: 1,
        }),
    }
}

/// Whether `at` is inside the half-open ratatui rectangle `area`.
fn contains(area: RatatuiRect, at: Point) -> bool {
    at.x >= area.x
        && at.y >= area.y
        && u32::from(at.x) < u32::from(area.x) + u32::from(area.width)
        && u32::from(at.y) < u32::from(area.y) + u32::from(area.height)
}

/// The side of `rect`'s one-cell border ring that `point` lies on. `point` is
/// assumed to be within `rect` but not within its inner content area. A corner
/// cell resolves to its vertical side, so a border drag on a corner reads as the
/// left or right edge.
fn border_side(rect: Rect, point: Point) -> Direction {
    let right = rect.origin.x + rect.size.cols - 1;
    let bottom = rect.origin.y + rect.size.rows - 1;
    if point.x == rect.origin.x {
        Direction::Left
    } else if point.x == right {
        Direction::Right
    } else if point.y == rect.origin.y {
        Direction::Up
    } else {
        debug_assert_eq!(point.y, bottom, "border cell is on one of the four edges");
        Direction::Down
    }
}

#[cfg(test)]
mod tests;
