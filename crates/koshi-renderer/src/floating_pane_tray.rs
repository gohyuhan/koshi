//! Where the floating pane tray and its tabline button sit on screen.
//!
//! The tabline draws the tray button ` ⧉ ` left of the mode tag. The tray
//! hangs from the top row of the pane area, and its right border sits under
//! the button's last cell. It draws the floating chrome
//! ([`crate::floating_chrome`]) with the close button only. Its titlebar holds
//! the title of the hovered or selected tile and, while the session holds a
//! floating pane, the label `minimize all` or `restore all`. The slot right
//! after the last floating pane shows the `+ new` tile. A tray of six slots
//! per row, with three floating panes and tile `0` selected:
//!
//! ```text
//! ┌───────────────────────────────────────────────────────────────────┐   row 0: top border
//! │ htop                                             minimize all  ✕  │   row 1: titlebar
//! ├───────────────────────────────────────────────────────────────────┤   row 2: separator
//! │ ┌ 1 ─────┐ ┌ 2 ─────┐ ┌ 3 ─────┐ ┌────────┐ ┌────────┐ ┌────────┐ │   rows 3 to 5: slots 0 to 5
//! │ │ htop   │ │ logs   │ │ vim    │ │ + new  │ │        │ │        │ │
//! │ └────────┘ └────────┘ └────────┘ └────────┘ └────────┘ └────────┘ │
//! │ ┌────────┐ ┌────────┐ ┌────────┐ ┌────────┐ ┌────────┐ ┌────────┐ │   rows 6 to 8: slots 6 to 11
//! │ │        │ │        │ │        │ │        │ │        │ │        │ │
//! │ └────────┘ └────────┘ └────────┘ └────────┘ └────────┘ └────────┘ │
//! └───────────────────────────────────────────────────────────────────┘   row 9: bottom border
//! ```
//!
//! Every rect here is in screen cells, `(0, 0)` at the viewport's top-left
//! cell.

use koshi_core::constant::{FLOATING_CHROME_SIZE, MAX_FLOATING_PANES_PER_SESSION};
use koshi_core::geometry::{Point, Rect, Size};

use crate::floating_chrome::{compute_floating_titlebar, FloatingTitlebarControls};
use crate::hit_test::{compute_frame_pane_area, compute_tabline_area, compute_viewport_area};
use crate::render::{
    compute_right_block_start_column, find_floating_pane_tray_button_column, get_text_width,
    FLOATING_PANE_TRAY_BUTTON_COLUMN_COUNT,
};
use crate::snapshot::{FrameLayout, SessionSnapshot};

/// The slots a tray holds: one per floating pane a session can hold.
pub const FLOATING_PANE_TRAY_SLOT_COUNT: usize = MAX_FLOATING_PANES_PER_SESSION;

/// The titlebar label while the viewer shows a floating pane.
const MINIMIZE_ALL_LABEL_TEXT: &str = "minimize all";

/// The titlebar label while the viewer shows no floating pane.
const RESTORE_ALL_LABEL_TEXT: &str = "restore all";

/// The blank cells between the tile title and the titlebar label.
const TITLE_LABEL_GAP_COLUMN_COUNT: u16 = 2;

/// The columns one slot takes: its border ring and eight cells inside it.
const SLOT_COLUMN_COUNT: u16 = 10;

/// The rows one slot takes: its top border, its title row and its bottom
/// border.
const SLOT_ROW_COUNT: u16 = 3;

/// The blank columns between two slots of one row.
const SLOT_GAP_COLUMN_COUNT: u16 = 1;

/// The columns on each side of the slots: the tray's border and one blank
/// column.
const TRAY_SIDE_COLUMN_COUNT: u16 = 2;

/// The most slots one row holds.
const MAXIMUM_SLOTS_PER_ROW: u16 = 6;

/// Where the parts of an open tray sit, in screen cells.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FloatingPaneTrayLayout {
    /// The tray's box, its floating chrome included.
    pub tray_rect: Rect,
    /// The box of each slot, slot `0` first: left to right, then top to
    /// bottom. Slot `i` shows the session's floating pane `i` while the
    /// session holds one, and the slot right after the last floating pane
    /// shows the `+ new` tile.
    pub slot_rects: [Rect; FLOATING_PANE_TRAY_SLOT_COUNT],
    /// The slots in each row: `6`, or fewer when the pane area left of the
    /// tray's right border is narrower than a six-slot tray (`69` columns).
    pub slots_per_row: u16,
    /// The titlebar cells that the hovered or selected tile's title may take.
    /// `None` when the titlebar has no title cell.
    pub tile_title_rect: Option<Rect>,
    /// The titlebar label `minimize all` or `restore all`, right-aligned in
    /// the titlebar's title cells. `None` while the session holds no floating
    /// pane, and when the label and the two blank cells before it leave no
    /// title cell.
    pub toggle_minimize_all_rect: Option<Rect>,
}

/// The tray button ` ⧉ ` on the tabline row: three cells left of the mode
/// tag. `None` when the frame draws no tabline (a zero-size viewport or
/// tabline region, or every pane suppressed), or the row leaves fewer than
/// three cells left of the mode tag.
///
/// An `80`-column viewport whose mode tag ` BASE ` starts at column `74` gives
/// `(71, 0)` `3x1`.
#[must_use]
pub fn find_floating_pane_tray_button_rect(frame_layout: FrameLayout<'_>) -> Option<Rect> {
    let viewport_area = compute_viewport_area(frame_layout);
    if viewport_area.width == 0
        || viewport_area.height == 0
        || frame_layout
            .session_snapshot
            .active_tab_snapshot
            .is_every_pane_suppressed
    {
        return None;
    }
    let tabline_area = compute_tabline_area(frame_layout, viewport_area);
    if tabline_area.width == 0 || tabline_area.height == 0 {
        return None;
    }
    let button_column = find_floating_pane_tray_button_column(
        tabline_area,
        compute_right_block_start_column(frame_layout.get_tabline_inputs(), tabline_area),
    )?;
    Some(Rect::from_origin_and_size(
        Point {
            column: button_column,
            row: tabline_area.y,
        },
        Size {
            column_count: FLOATING_PANE_TRAY_BUTTON_COLUMN_COUNT,
            row_count: 1,
        },
    ))
}

/// The cell of the tray button's glyph `⧉`: the button's middle cell. `None`
/// when [`find_floating_pane_tray_button_rect`] gives `None`. A button at
/// `(71, 0)` gives `(72, 0)`.
#[must_use]
pub fn find_floating_pane_tray_glyph_cell(frame_layout: FrameLayout<'_>) -> Option<Point> {
    let button_rect = find_floating_pane_tray_button_rect(frame_layout)?;
    Some(Point {
        column: button_rect.origin.column + 1,
        row: button_rect.origin.row,
    })
}

/// Where the tray sits under the tray button in `frame_layout`, open or not.
///
/// The tray's right border is the button's last column, and its top border is
/// the pane area's top row. Its row holds the most slots, at most `6`, that
/// fit between the pane area's left edge and that border, and its rows hold
/// all [`FLOATING_PANE_TRAY_SLOT_COUNT`] slots. A row of `n` slots is
/// `3 + 11n` columns wide, and `r` rows of slots, under the titlebar and the
/// separator, are `4 + 3r` rows tall.
///
/// The titlebar holds the title cells and the close button of the floating
/// chrome ([`crate::floating_chrome`]). The label is right-aligned in the
/// title cells, and the tile title takes the title cells left of it, less two
/// blank cells. The label is `restore all` while the viewer minimized every
/// floating pane, `minimize all` while it shows one, and absent while the
/// session holds no floating pane. With no label, the tile title takes every
/// title cell.
///
/// `None` when the button is not drawn
/// ([`find_floating_pane_tray_button_rect`]), the pane area left of the border
/// is narrower than a one-slot tray (`14` columns), or the pane area is
/// shorter than the tray.
///
/// A button at `(71, 0)` over a pane area at `(0, 1)` `80x22` gives a six-slot
/// tray at `(5, 1)` `69x10`, slot `0` at `(7, 4)` `10x3`, slot `7` at
/// `(18, 7)`, the tile title at `(7, 2)` `47x1` and the label `minimize all`
/// at `(56, 2)` `12x1`.
#[must_use]
pub fn compute_floating_pane_tray_layout(
    frame_layout: FrameLayout<'_>,
) -> Option<FloatingPaneTrayLayout> {
    let button_rect = find_floating_pane_tray_button_rect(frame_layout)?;
    let pane_area = compute_frame_pane_area(frame_layout, compute_viewport_area(frame_layout));
    let tray_end_column = button_rect.origin.column + button_rect.size.column_count;
    if tray_end_column > pane_area.right() {
        return None;
    }
    let tray_room_column_count = tray_end_column.saturating_sub(pane_area.x);
    let slots_per_row = (1..=MAXIMUM_SLOTS_PER_ROW).rev().find(|&slots_per_row| {
        compute_tray_column_count(slots_per_row) <= tray_room_column_count
    })?;
    let slot_row_count = FLOATING_PANE_TRAY_SLOT_COUNT.div_ceil(usize::from(slots_per_row)) as u16;
    let tray_size = Size {
        column_count: compute_tray_column_count(slots_per_row),
        row_count: FLOATING_CHROME_SIZE.row_count + slot_row_count * SLOT_ROW_COUNT,
    };
    if tray_size.row_count > pane_area.height {
        return None;
    }
    let tray_rect = Rect::from_origin_and_size(
        Point {
            column: tray_end_column - tray_size.column_count,
            row: pane_area.y,
        },
        tray_size,
    );

    let slot_rects = std::array::from_fn(|slot_index| {
        let slot_index = slot_index as u16;
        let slot_column_index = slot_index % slots_per_row;
        let slot_row_index = slot_index / slots_per_row;
        Rect::from_origin_and_size(
            Point {
                column: tray_rect.origin.column
                    + TRAY_SIDE_COLUMN_COUNT
                    + slot_column_index * (SLOT_COLUMN_COUNT + SLOT_GAP_COLUMN_COUNT),
                row: tray_rect.origin.row
                    + (FLOATING_CHROME_SIZE.row_count - 1)
                    + slot_row_index * SLOT_ROW_COUNT,
            },
            Size {
                column_count: SLOT_COLUMN_COUNT,
                row_count: SLOT_ROW_COUNT,
            },
        )
    });

    let titlebar =
        compute_floating_titlebar(tray_rect, FloatingTitlebarControls::CLOSE_BUTTON_ONLY);
    let label_column_count =
        select_toggle_minimize_all_label_text(frame_layout.session_snapshot).map(get_text_width);
    let (tile_title_rect, toggle_minimize_all_rect) =
        match (titlebar.title_rect, label_column_count) {
            (Some(title_rect), Some(label_column_count))
                if title_rect.size.column_count
                    > label_column_count + TITLE_LABEL_GAP_COLUMN_COUNT =>
            {
                let tile_title_column_count = title_rect.size.column_count
                    - label_column_count
                    - TITLE_LABEL_GAP_COLUMN_COUNT;
                (
                    Some(Rect::from_origin_and_size(
                        title_rect.origin,
                        Size {
                            column_count: tile_title_column_count,
                            row_count: 1,
                        },
                    )),
                    Some(Rect::from_origin_and_size(
                        Point {
                            column: title_rect.origin.column + title_rect.size.column_count
                                - label_column_count,
                            row: title_rect.origin.row,
                        },
                        Size {
                            column_count: label_column_count,
                            row_count: 1,
                        },
                    )),
                )
            }
            (title_rect, _) => (title_rect, None),
        };
    Some(FloatingPaneTrayLayout {
        tray_rect,
        slot_rects,
        slots_per_row,
        tile_title_rect,
        toggle_minimize_all_rect,
    })
}

/// The tray this frame draws: [`compute_floating_pane_tray_layout`] while
/// `frame_layout.viewer_chrome.floating_pane_tray` is `Some`, and `None` while
/// the viewer's tray is closed.
#[must_use]
pub fn find_drawn_floating_pane_tray_layout(
    frame_layout: FrameLayout<'_>,
) -> Option<FloatingPaneTrayLayout> {
    frame_layout.viewer_chrome.floating_pane_tray?;
    compute_floating_pane_tray_layout(frame_layout)
}

/// The tray's titlebar label: `restore all` while the viewer minimized every
/// floating pane of `session_snapshot`
/// ([`SessionSnapshot::is_every_floating_pane_minimized`]), and
/// `minimize all` otherwise. `None` while the session holds no floating pane.
#[must_use]
pub(crate) fn select_toggle_minimize_all_label_text(
    session_snapshot: &SessionSnapshot,
) -> Option<&'static str> {
    if session_snapshot.floating_panes.is_empty() {
        return None;
    }
    if session_snapshot.is_every_floating_pane_minimized() {
        Some(RESTORE_ALL_LABEL_TEXT)
    } else {
        Some(MINIMIZE_ALL_LABEL_TEXT)
    }
}

/// The tiles a tray shows for `floating_pane_count` floating panes: one per
/// floating pane, and the `+ new` tile in the next slot while a slot is free.
/// `0` gives `1`, `3` gives `4`, and
/// [`FLOATING_PANE_TRAY_SLOT_COUNT`] gives [`FLOATING_PANE_TRAY_SLOT_COUNT`].
#[must_use]
pub fn compute_tray_tile_count(floating_pane_count: usize) -> usize {
    (floating_pane_count + 1).min(FLOATING_PANE_TRAY_SLOT_COUNT)
}

/// The columns of a tray whose rows hold `slots_per_row` slots: the two side
/// strips, the slots and the gaps between them. `6` gives `69`, and `1` gives
/// `14`.
fn compute_tray_column_count(slots_per_row: u16) -> u16 {
    2 * TRAY_SIDE_COLUMN_COUNT
        + slots_per_row * SLOT_COLUMN_COUNT
        + (slots_per_row - 1) * SLOT_GAP_COLUMN_COUNT
}

#[cfg(test)]
pub(crate) mod tests;
