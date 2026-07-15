//! Mouse hit-testing: map a client-local screen cell to the UI region under it.
//!
//! A decoded mouse event carries a cell coordinate in the client's own screen
//! space (`(0, 0)` top-left, `x` rightward, `y` downward). Before koshi can act
//! on a click — focus a pane, drag a border, forward to a program — it must know
//! *what* that cell sits on. [`hit_test`] answers that against one frozen
//! [`RenderSnapshot`], returning a [`HitRegion`] label. It only classifies; it
//! never changes state and never forwards anything.
//!
//! The frame is read the same way [`crate::render`] draws it, so the region a
//! click lands on is the region that was painted there:
//!
//! - The **tabline** (top row) and the **hint bar** (bottom row) are koshi-owned
//!   chrome painted last, over whatever lies beneath, so a click on those rows
//!   is chrome, not the pane under it.
//! - The rest is the **pane area**: the solved layout centered in the viewport,
//!   with a dim letterbox margin around it when the client is larger than the
//!   size the layout was solved for. A click in that margin hits nothing.
//! - Inside the pane area, a pane's one-cell **border** ring is distinct from its
//!   **content**; a collapsed stack member's title strip hit-tests like a border.

use koshi_core::geometry::{Direction, Point, Rect};
use koshi_core::ids::{PaneId, TabId};
use ratatui::layout::Rect as RatatuiRect;

use crate::render::{content_rect, tabline_layout};
use crate::snapshot::RenderSnapshot;

/// The UI region under a client-local screen cell, as classified by
/// [`hit_test`].
///
/// Every variant names a region the renderer actually draws this frame; the
/// caller decides what a click on each one does (focus, resize, forward — all in
/// later steps). [`None`](HitRegion::None) is the letterbox margin, the
/// too-small overlay, or a degenerate viewport: nothing to act on.
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

/// Classify the client-local screen cell `at` against the frozen frame
/// `snapshot`.
///
/// Reads the frame in the renderer's own paint order so chrome wins over the
/// pane content beneath it: the tabline (top row) and hint bar (bottom row) are
/// tested before the pane area, and the pane area is the layout centered inside
/// the viewport with a letterbox margin that hits nothing.
#[must_use]
pub fn hit_test(snapshot: &RenderSnapshot, at: Point) -> HitRegion {
    let viewport = snapshot.client.viewport;
    if viewport.cols == 0 || viewport.rows == 0 {
        return HitRegion::None;
    }

    let tab = &snapshot.session.active_tab;
    // No room for any pane: the whole frame is the too-small overlay, and no
    // chrome or pane is drawn, so nothing is hit-testable.
    if tab.all_suppressed {
        return HitRegion::None;
    }

    let area = RatatuiRect {
        x: 0,
        y: 0,
        width: viewport.cols,
        height: viewport.rows,
    };

    // Chrome rows are painted last and cover the pane area beneath them, so a
    // click on those rows is chrome regardless of what the layout put there.
    if at.y == area.y {
        return tabline_region(snapshot, area, at.x);
    }
    if viewport.rows >= 2 && at.y == area.bottom() - 1 {
        return HitRegion::Statusline;
    }

    // The pane area: the effective-sized layout centered in the viewport. A
    // cell outside it is letterbox margin.
    let content = content_rect(area, tab.effective_size);
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
fn tabline_region(snapshot: &RenderSnapshot, area: RatatuiRect, x: u16) -> HitRegion {
    let layout = tabline_layout(snapshot, area);
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
                tab_id: snapshot.session.tabs_metadata[meta_index].id,
            };
        }
    }
    HitRegion::Tabline
}

/// The metadata index of the first tab currently visible in `snapshot`'s
/// tabline window.
///
/// The mouse-routing layer reads this to anchor a peek-drag and to step the
/// window on a wheel scroll, resolving the same window the renderer draws and
/// [`hit_test`] classifies — the tab-strip solve lives in one place.
#[must_use]
pub fn tabline_first_visible(snapshot: &RenderSnapshot) -> usize {
    let viewport = snapshot.client.viewport;
    let area = RatatuiRect {
        x: 0,
        y: 0,
        width: viewport.cols,
        height: viewport.rows,
    };
    tabline_layout(snapshot, area).first_visible
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
