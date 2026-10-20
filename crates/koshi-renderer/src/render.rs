//! Frame composition.
//!
//! [`render_frame`] paints one [`RenderSnapshot`] into a ratatui
//! [`Buffer`] as three zones: a **tabline** (session name, the running
//! koshi version, and the tab list on the left, the right-aligned mode tag),
//! the **pane area** (a bordered box per visible pane, the focused pane's
//! border highlighted), and the **statusline** — a koshi-owned row with
//! per-mode keybinding hints. Placement previews tint the moving and destination
//! panes separately, color their outlines, and explain their proposed positions.
//! The committed region solve supplies all three zones.
//!
//! Collapsed members of a stacked pane group are drawn as one-row title strips
//! in the pane area, and each visible terminal pane's cells are painted into its
//! content rect. The focused pane's cursor cell is reported separately by
//! [`get_cursor_position`] for the caller to place the terminal's hardware cursor;
//! the buffer itself carries no cursor. When the active tab has no room
//! for any pane, a centered "terminal too small" overlay replaces the pane
//! render for that frame. When the pane area is larger than the size the
//! layout was solved for, the layout is centered inside that pane area and the
//! surrounding margin is filled with a dim letterbox.
//!
//! The floating panes the viewing client shows draw over the tiled panes and
//! the letterbox, back to front, each in a box with a titlebar. They sit in
//! the pane area at their own cells, with no letterbox offset.
//!
//! Each drawn cell takes the columns that [`compute_text_display_width`] gives
//! its symbol, and the text of the tabline, the statusline, the borders and
//! the titlebars is placed at those columns. A cell whose symbol ratatui
//! measures at another width gets that width as its forced width: a cell that
//! holds `U+17D8` takes 1 column, not the 3 that ratatui measures.

use std::num::NonZeroU16;
use std::sync::Arc;

use ratatui::buffer::{Buffer, CellDiffOption, CellWidth};
use ratatui::layout::{Position, Rect as RatatuiRect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use koshi_core::command::{PanePlacementAnchor, PanePlacementTarget};
use koshi_core::display_width::compute_text_display_width;
use koshi_core::geometry::{Point, Rect, Size};
use koshi_core::ids::{PaneId, TabId};
use koshi_core::key::KeySequence;
use koshi_core::lock::LockMode;
use koshi_terminal::style::{Color as CellColor, Style as CellStyle, UnderlineStyle};

use crate::hit_test::{compute_placement_handle_rect, PLACEMENT_HANDLE_COLUMN_COUNT};
use crate::images::{
    compute_floating_pane_image_placeholder_rects, compute_image_placeholder_rects,
    draw_image_placeholders, ImagePlacementKey, ImageRenderMode,
};
use crate::region::StatuslineInputs;
use crate::snapshot::{
    CommittedRegions, CursorStyle, GridView, KeymapHints, PanePlacementPresentation, PaneSnapshot,
    Reconnecting, RenderSnapshot, SelectionSpans, ViewerChrome,
};
use crate::statusline_hints::draw_statusline;
use crate::theme::Theme;

const PLACEMENT_HOVER_TINT_COLOR: Color = Color::Rgb(0x3a, 0x3a, 0x3a);

mod floating;
mod placement;

use floating::draw_floating_pane;
pub(crate) use floating::{list_placed_floating_panes, PlacedFloatingPane};
use placement::draw_pane_placement_presentation;

/// The rows of the buffer that one [`render_frame`] call draws.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DrawnRows<'rows> {
    /// Every row of the viewport.
    Every,
    /// The listed buffer rows, in ascending order with none repeated. Every
    /// cell of a listed row ends as a call with [`DrawnRows::Every`] leaves
    /// it. A cell of any other row ends in any state. Example: `Listed(&[3,
    /// 9])` draws rows 3 and 9 whole.
    Listed(&'rows [u16]),
}

impl<'rows> DrawnRows<'rows> {
    /// Whether the row `row_index` is drawn. [`DrawnRows::Every`] gives
    /// `true` for every row.
    fn is_row_drawn(self, row_index: u16) -> bool {
        match self {
            DrawnRows::Every => true,
            DrawnRows::Listed(listed_rows) => listed_rows.binary_search(&row_index).is_ok(),
        }
    }

    /// Whether a row of `area`, from its top up to its bottom, is drawn.
    /// [`DrawnRows::Every`] gives `true` for every area. Example: rows `[3,
    /// 9]` and an area of rows 4 to 8 give `false`.
    fn is_area_drawn(self, area: RatatuiRect) -> bool {
        self.list_rows_between(area.top(), area.bottom())
            .next()
            .is_some()
    }

    /// The drawn rows from `top_row` up to, not including, `bottom_row`, top
    /// to bottom. Example: rows `[3, 9]` between `2` and `9` give `3`.
    fn list_rows_between(self, top_row: u16, bottom_row: u16) -> impl Iterator<Item = u16> + 'rows {
        (top_row..bottom_row).filter(move |row_index| self.is_row_drawn(*row_index))
    }
}

/// Paint `render_snapshot` into `screen_buffer` over `viewport_area`.
///
/// It does nothing for a zero-size area. When the active tab has no room for any
/// pane (`is_every_pane_suppressed`), it blanks `viewport_area`, draws a centered too-small
/// overlay, and returns, skipping the panes and both chrome rows.
///
/// Otherwise paints in this order:
///
/// 1. Blanks every cell of `viewport_area`.
/// 2. Draws one bordered box per visible pane: the terminal title in the top
///    border, and the scroll position in the bottom border when the pane is
///    scrolled back.
/// 3. Draws each visible terminal pane's cells into its content rect, and
///    tints the content of the pane whose placement handle the pointer rests
///    on.
/// 4. Keeps pane cells under native images or writes the unavailable-image
///    text over unavailable coverage.
/// 5. Draws the one-row title strip for every collapsed stack member.
/// 6. Tints each visible affected pane with a role color and colors its outline.
///    The moving pane and the uncovered area of one visible destination pane
///    show preview text when both areas are visible.
/// 7. Fills the letterbox margin: every cell of `viewport_area` outside the centered
///    layout, the chrome rows included.
/// 8. Draws the floating panes this viewer draws, back to front, over every
///    cell drawn before them: each one's chrome and cells, then the
///    unavailable-image text over its images. They sit in the pane area with
///    no letterbox offset. None is drawn while
///    `viewer_chrome.is_pane_placement_visible` is `true` or every pane of the
///    active tab is suppressed.
/// 9. Draws the tabline in the first committed region, over that margin.
/// 10. Draws the statusline in the second committed region, over that margin.
/// 11. Gives each cell of `viewport_area` whose symbol ratatui measures at
///     another width than [`compute_text_display_width`] gives it that width as
///     its forced width. Example: a cell that holds `U+17D8` takes 1 column, not
///     the 3 that ratatui measures.
///
/// `theme`, `keymap_hints`, `pending_key_sequence`, and `viewer_chrome` come
/// from the viewer: the colors it paints koshi's chrome in, the statusline data
/// for the mode it is in, the multi-chord sequence it has open, and the pane its
/// pointer is over together with where its tab strip is scrolled and whether it
/// is dialing the session again.
/// The attached client passes the same [`CommittedRegions`] and
/// `viewer_chrome` to this function and to [`get_cursor_position`]. For
/// example, a left region of 20 columns on a `120x40` viewport leaves the pane
/// rectangle at `x = 20`.
///
/// `image_render_mode` `Placeholder` writes `terminal image unavailable` into visible
/// image rectangles: a four-column image at `(12, 6)` shows `term` across its
/// first four cells. `Native` keeps the pane cells beneath image pixels. In
/// `Native` mode, an image shows the unavailable-image text while its record
/// is missing or `unavailable_image_keys` names its placement. A `Native`
/// image that a floating pane drawn above its pane covers shows no text.
///
/// `placement_presentation` tints visible affected panes and paints messages
/// inside visible pane content outside stack headers. Collapsed stack headers
/// keep their text under the tint. The moving pane's tint and text take priority
/// where pane rectangles overlap outside those headers. `placement_target`
/// selects destination border styling before the preview colors affected panes.
///
/// `drawn_rows` [`DrawnRows::Every`] runs every step above over every row.
/// [`DrawnRows::Listed`] leaves each listed row as `Every` would, and runs
/// each step that writes whole rows (steps 1, 3, 7, 8's cells and 11) only
/// over the listed rows. A box, strip or bar that covers no listed row is
/// not drawn. Example: rows `[10]` of a frame whose one pane prints on row
/// 10 redraws the 200 cells of row 10, and the pane's border cells on that
/// row.
///
/// # Panics
///
/// In a debug build, when `render_snapshot.client_snapshot.active_tab_id` is not
/// `render_snapshot.session_snapshot.active_tab_snapshot.tab_id`.
#[allow(clippy::too_many_arguments)]
pub fn render_frame(
    render_snapshot: &RenderSnapshot,
    committed_regions: &CommittedRegions,
    theme: &Theme,
    keymap_hints: &KeymapHints,
    pending_key_sequence: Option<&KeySequence>,
    viewer_chrome: ViewerChrome,
    image_render_mode: ImageRenderMode,
    unavailable_image_keys: &[ImagePlacementKey],
    placement_presentation: Option<&PanePlacementPresentation>,
    placement_target: Option<&PanePlacementTarget>,
    viewport_area: RatatuiRect,
    drawn_rows: DrawnRows<'_>,
    screen_buffer: &mut Buffer,
) {
    if viewport_area.width == 0 || viewport_area.height == 0 {
        return;
    }

    // `session_snapshot.active_tab_snapshot` is the tab this client views: its
    // id equals the client's `active_tab_id`.
    debug_assert_eq!(
        render_snapshot.client_snapshot.active_tab_id,
        render_snapshot
            .session_snapshot
            .active_tab_snapshot
            .tab_id,
        "snapshot builder must solve the client's active tab into session_snapshot.active_tab_snapshot"
    );

    // Reset every cell of each drawn row of `viewport_area` first.
    clear_drawn_rows(viewport_area, drawn_rows, screen_buffer);

    // No room for any pane: the whole frame becomes the too-small overlay.
    if render_snapshot
        .session_snapshot
        .active_tab_snapshot
        .is_every_pane_suppressed
    {
        draw_too_small_overlay(viewport_area, screen_buffer);
        return;
    }

    // Center the solved layout inside this client's viewport. The layout was
    // solved for the tab's effective (smallest-client) size: a larger client
    // has margin. `effective_layout_rect` is that effective-sized rect centered
    // in the pane area left by the committed regions, and `layout_origin` shifts each
    // effective-space layout rect into it.
    let effective_layout_rect = compute_content_rect(
        compute_pane_area(committed_regions, viewport_area),
        render_snapshot
            .session_snapshot
            .active_tab_snapshot
            .tab_size,
    );
    let layout_origin = get_area_origin(effective_layout_rect);
    let placed_floating_panes = list_placed_floating_panes(
        render_snapshot,
        committed_regions,
        viewer_chrome,
        viewport_area,
    );

    draw_panes(
        render_snapshot,
        theme,
        viewer_chrome,
        placement_target,
        layout_origin,
        drawn_rows,
        screen_buffer,
    );
    draw_pane_contents(render_snapshot, layout_origin, drawn_rows, screen_buffer);
    draw_placement_hover_tints(
        render_snapshot,
        viewer_chrome,
        layout_origin,
        drawn_rows,
        screen_buffer,
    );
    let placeholder_rects = compute_image_placeholder_rects(
        render_snapshot,
        committed_regions,
        &placed_floating_panes,
        viewport_area,
        image_render_mode,
        unavailable_image_keys,
        placement_presentation,
    );
    draw_image_placeholders(&placeholder_rects, screen_buffer);
    draw_stack_headers(
        render_snapshot,
        theme,
        viewer_chrome,
        placement_target,
        layout_origin,
        drawn_rows,
        screen_buffer,
    );
    if let Some(placement_presentation) = placement_presentation {
        draw_pane_placement_presentation(
            render_snapshot,
            placement_presentation,
            layout_origin,
            theme,
            screen_buffer,
        );
    }

    // The margin fills first; the floating panes, the tabline and the
    // statusline paint over it.
    draw_letterbox(
        viewport_area,
        effective_layout_rect,
        theme,
        drawn_rows,
        screen_buffer,
    );

    // Back to front: each floating pane, then the unavailable-image text over
    // its images.
    for (floating_pane_index, placed_floating_pane) in placed_floating_panes.iter().enumerate() {
        draw_floating_pane(
            render_snapshot,
            placed_floating_pane,
            viewer_chrome,
            theme,
            drawn_rows,
            screen_buffer,
        );
        let floating_pane_placeholder_rects = compute_floating_pane_image_placeholder_rects(
            render_snapshot,
            placed_floating_pane,
            &placed_floating_panes[floating_pane_index + 1..],
            viewport_area,
            image_render_mode,
            unavailable_image_keys,
        );
        draw_image_placeholders(&floating_pane_placeholder_rects, screen_buffer);
    }

    // The tabline draws from the same tab-row facts hit-testing reads.
    if let Some(tabline_rect) = find_region_area(committed_regions, 0, viewport_area)
        .filter(|tabline_rect| drawn_rows.is_area_drawn(*tabline_rect))
    {
        let tabline_inputs = render_snapshot
            .build_frame_layout(viewer_chrome)
            .get_tabline_inputs();
        draw_tabline(tabline_inputs, theme, tabline_rect, screen_buffer);
    }

    if let Some(statusline_rect) = find_region_area(committed_regions, 1, viewport_area)
        .filter(|statusline_rect| drawn_rows.is_area_drawn(*statusline_rect))
    {
        draw_statusline(
            StatuslineInputs {
                keymap_hints,
                pending_key_sequence,
                is_recovery_notice_visible: render_snapshot.is_recovery_notice_visible,
            },
            theme,
            statusline_rect,
            screen_buffer,
        );
    }

    apply_display_widths(viewport_area, drawn_rows, screen_buffer);
}

/// The buffer rows that differ between the frame [`render_frame`] draws from
/// `previous_render_snapshot` and the one it draws from `next_render_snapshot`,
/// when every other input of the two calls is equal: in ascending order, none
/// repeated. `None` when the two snapshots differ in anything a frame draws
/// other than pane rows.
///
/// A pane row differs when the grid views of the two snapshots hold different
/// shared rows at its index ([`Arc::ptr_eq`]). That row lands on the buffer
/// row of its pane's content area, tiled or floating, when that area shows
/// it. Each pane's cursor, mouse modes, alternate screen state, top line
/// number and `has_selection` are drawn into no cell and may differ. Every
/// other field must be equal, each grid view must keep its row count, column
/// count and scroll offset, and each pane must show the same scroll position
/// text.
///
/// Example: one pane whose content area starts at buffer row 2, with grid row
/// 8 alone replaced, gives `[10]`.
#[must_use]
pub fn list_changed_screen_rows(
    previous_render_snapshot: &RenderSnapshot,
    next_render_snapshot: &RenderSnapshot,
    committed_regions: &CommittedRegions,
    viewer_chrome: ViewerChrome,
    viewport_area: RatatuiRect,
) -> Option<Vec<u16>> {
    let RenderSnapshot {
        is_recovery_notice_visible,
        session_snapshot,
        pane_snapshots,
        client_snapshot,
    } = next_render_snapshot;
    if previous_render_snapshot.is_recovery_notice_visible != *is_recovery_notice_visible
        || previous_render_snapshot.session_snapshot != *session_snapshot
        || previous_render_snapshot.client_snapshot != *client_snapshot
        || previous_render_snapshot.pane_snapshots.len() != pane_snapshots.len()
    {
        return None;
    }
    let layout_origin =
        compute_layout_origin(next_render_snapshot, committed_regions, viewport_area);
    let placed_floating_panes = list_placed_floating_panes(
        next_render_snapshot,
        committed_regions,
        viewer_chrome,
        viewport_area,
    );
    let mut changed_screen_rows = Vec::new();
    for (previous_pane_snapshot, next_pane_snapshot) in previous_render_snapshot
        .pane_snapshots
        .iter()
        .zip(pane_snapshots)
    {
        if !is_pane_drawn_alike_except_rows(previous_pane_snapshot, next_pane_snapshot) {
            return None;
        }
        let (Some(previous_grid_view), Some(next_grid_view)) = (
            &previous_pane_snapshot.terminal_grid_view,
            &next_pane_snapshot.terminal_grid_view,
        ) else {
            continue;
        };
        let pane_id = next_pane_snapshot.pane_id;
        let tiled_content_area = session_snapshot
            .active_tab_snapshot
            .pane_slots
            .iter()
            .find(|pane_slot| pane_slot.pane_id == pane_id && pane_slot.is_visible)
            .and_then(|pane_slot| pane_slot.content_rect)
            .map(|content_rect| place_cell_rect(content_rect, layout_origin));
        let floating_content_area = placed_floating_panes
            .iter()
            .find(|placed_floating_pane| placed_floating_pane.floating_pane_slot.pane_id == pane_id)
            .map(|placed_floating_pane| placed_floating_pane.visible_content_area);
        for content_area in [tiled_content_area, floating_content_area]
            .into_iter()
            .flatten()
        {
            let shown_area = content_area.intersection(viewport_area);
            let shown_row_count = next_grid_view
                .get_view_dimensions()
                .0
                .min(shown_area.height);
            for (screen_row, (previous_row_snapshot, next_row_snapshot)) in
                (shown_area.y..shown_area.y + shown_row_count).zip(
                    previous_grid_view
                        .row_snapshots
                        .iter()
                        .zip(&next_grid_view.row_snapshots),
                )
            {
                if !Arc::ptr_eq(previous_row_snapshot, next_row_snapshot) {
                    changed_screen_rows.push(screen_row);
                }
            }
        }
    }
    changed_screen_rows.sort_unstable();
    changed_screen_rows.dedup();
    Some(changed_screen_rows)
}

/// Whether `previous_pane_snapshot` and `next_pane_snapshot` draw the same
/// cells apart from their grid rows: the same pane, title, image placements,
/// reverse video, selection and scroll position text, and grid views with the
/// same row count, column count and scroll offset, or no grid view in both.
/// The fields drawn into no cell are not compared.
fn is_pane_drawn_alike_except_rows(
    previous_pane_snapshot: &PaneSnapshot,
    next_pane_snapshot: &PaneSnapshot,
) -> bool {
    let PaneSnapshot {
        pane_id,
        pane_title,
        cursor_snapshot: _,
        terminal_grid_view,
        image_placement_snapshots,
        is_reverse_video,
        mouse_tracking: _,
        is_alternate_scroll_enabled: _,
        is_on_alternate_screen: _,
        view_top_row_index: _,
        selection_spans,
        has_selection: _,
        scrollback_metadata: _,
    } = next_pane_snapshot;
    let is_grid_shape_alike = match (
        &previous_pane_snapshot.terminal_grid_view,
        terminal_grid_view,
    ) {
        (None, None) => true,
        (Some(previous_grid_view), Some(next_grid_view)) => {
            let GridView {
                row_snapshots,
                column_count,
                view_row_offset,
            } = next_grid_view;
            previous_grid_view.row_snapshots.len() == row_snapshots.len()
                && previous_grid_view.column_count == *column_count
                && previous_grid_view.view_row_offset == *view_row_offset
        }
        (None, Some(_)) | (Some(_), None) => false,
    };
    is_grid_shape_alike
        && previous_pane_snapshot.pane_id == *pane_id
        && previous_pane_snapshot.pane_title == *pane_title
        && previous_pane_snapshot.image_placement_snapshots == *image_placement_snapshots
        && previous_pane_snapshot.is_reverse_video == *is_reverse_video
        && previous_pane_snapshot.selection_spans == *selection_spans
        && get_pane_scroll(previous_pane_snapshot) == get_pane_scroll(next_pane_snapshot)
}

/// Reset every cell of `clear_area` inside the buffer on each row that
/// `drawn_rows` draws.
fn clear_drawn_rows(
    clear_area: RatatuiRect,
    drawn_rows: DrawnRows<'_>,
    screen_buffer: &mut Buffer,
) {
    let clear_area = clear_area.intersection(screen_buffer.area);
    for row_index in drawn_rows.list_rows_between(clear_area.top(), clear_area.bottom()) {
        for column_index in clear_area.left()..clear_area.right() {
            screen_buffer[(column_index, row_index)].reset();
        }
    }
}

/// The buffer cell where the client's focused pane wants the hardware cursor, or
/// `None` when no cursor should show this frame.
///
/// Companion to [`render_frame`]: the buffer carries no cursor, so the caller
/// reads this alongside the paint — passing the same `viewport_area`,
/// committed regions and `viewer_chrome` — and places the terminal's cursor at
/// the returned [`Position`] (or hides it on `None`). The position is the
/// focused pane's cursor cell — its row and column within the pane's content
/// area, clamped to that content area's last cell — in the same absolute
/// buffer coordinates the panes are drawn in. A tiled pane's content area takes
/// the letterbox offset `render_frame` centers the layout with; a floating
/// pane's takes none.
///
/// Returns `None` when the client has no focused pane; that pane is neither a
/// floating pane this viewer draws nor a placed slot of the active tab; it has
/// no content snapshot; it is not visible or has no content area (suppressed,
/// hidden, a collapsed stack member, or a slot of two or fewer columns or rows,
/// whose content rect holds no cells); it has no terminal grid this frame; its
/// view is scrolled back into history; the application has hidden its cursor;
/// the cursor cell of a floating pane lies outside the pane area; or a floating
/// pane drawn above the focused pane covers the cursor cell.
///
/// A `120x40` viewport with a 20-column left region places the same pane cursor
/// 20 columns farther right than a whole-area layout. A focused floating pane
/// whose content starts at `(10, 3)` in a pane area at `(0, 1)` puts a cursor
/// at row `2`, column `5` at `(15, 6)`.
pub fn get_cursor_position(
    render_snapshot: &RenderSnapshot,
    committed_regions: &CommittedRegions,
    viewer_chrome: ViewerChrome,
    viewport_area: RatatuiRect,
) -> Option<Position> {
    let focused_pane_id = render_snapshot.client_snapshot.focused_pane_id?;
    let placed_floating_panes = list_placed_floating_panes(
        render_snapshot,
        committed_regions,
        viewer_chrome,
        viewport_area,
    );

    // The content area the cursor sits in, the part of it that is drawn, and
    // the floating panes drawn above it.
    let focused_floating_pane_index =
        placed_floating_panes
            .iter()
            .position(|placed_floating_pane| {
                placed_floating_pane.floating_pane_slot.pane_id == focused_pane_id
            });
    let (content_area, visible_content_area, covering_floating_panes) =
        match focused_floating_pane_index {
            Some(floating_pane_index) => {
                let placed_floating_pane = &placed_floating_panes[floating_pane_index];
                (
                    placed_floating_pane.content_area,
                    placed_floating_pane.visible_content_area,
                    &placed_floating_panes[floating_pane_index + 1..],
                )
            }
            None => {
                let pane_slot = render_snapshot
                    .session_snapshot
                    .active_tab_snapshot
                    .pane_slots
                    .iter()
                    .find(|pane_slot| pane_slot.pane_id == focused_pane_id)?;
                if !pane_slot.is_visible {
                    return None;
                }
                // `content_rect` is in tab-layout space; `place_cell_rect`
                // shifts it by the same letterbox offset `render_frame`
                // centers with.
                let placed_content_rect = place_cell_rect(
                    pane_slot.content_rect?,
                    compute_layout_origin(render_snapshot, committed_regions, viewport_area),
                );
                (
                    placed_content_rect,
                    placed_content_rect,
                    &placed_floating_panes[..],
                )
            }
        };
    if content_area.width == 0 || content_area.height == 0 {
        return None;
    }

    let pane_snapshot = render_snapshot.find_pane_snapshot(focused_pane_id)?;
    let terminal_grid_view = pane_snapshot.terminal_grid_view.as_ref()?;
    // A view scrolled back into history shows no hardware cursor.
    if terminal_grid_view.view_row_offset > 0 {
        return None;
    }
    if !pane_snapshot.cursor_snapshot.is_visible {
        return None;
    }

    // The content area's origin plus the pane-local column/row is the screen
    // position, clamped to the content area's last cell.
    let screen_column = (content_area.x + pane_snapshot.cursor_snapshot.column_index)
        .min(content_area.right().saturating_sub(1));
    let screen_row = (content_area.y + pane_snapshot.cursor_snapshot.row_index)
        .min(content_area.bottom().saturating_sub(1));
    let cursor_position = Position::new(screen_column, screen_row);
    if !visible_content_area.contains(cursor_position)
        || covering_floating_panes
            .iter()
            .any(|placed_floating_pane| placed_floating_pane.outer_area.contains(cursor_position))
    {
        return None;
    }
    Some(cursor_position)
}

/// How the outer terminal's cursor should look this frame:
/// [`Shaped`](CursorStyle::Shaped) with what the focused pane asked for via
/// DECSCUSR, or [`UserDefault`](CursorStyle::UserDefault) when it asked for
/// nothing — a plain shell never sends DECSCUSR, so its cursor stays whatever
/// the user configured.
///
/// `None` — meaning "leave the cursor as it is" — when the client has no
/// focused pane, or the focused pane has no content snapshot or no terminal
/// grid this frame.
///
/// Companion to [`get_cursor_position`], which says where the cursor goes; this
/// says what it looks like once it is there. The caller applies it to the outer
/// terminal with crossterm's `SetCursorStyle`: vim's insert-mode bar then
/// shows as a bar, not a block.
///
/// Not gated on the cursor being visible or the view being scrolled back.
#[must_use]
pub fn get_cursor_style(render_snapshot: &RenderSnapshot) -> Option<CursorStyle> {
    let pane_snapshot =
        render_snapshot.find_pane_snapshot(render_snapshot.client_snapshot.focused_pane_id?)?;
    pane_snapshot.terminal_grid_view.as_ref()?;
    let cursor_style = match pane_snapshot.cursor_snapshot.shape {
        Some(cursor_shape) => CursorStyle::Shaped {
            shape: cursor_shape,
            is_blinking: pane_snapshot.cursor_snapshot.is_blinking,
        },
        None => CursorStyle::UserDefault,
    };
    Some(cursor_style)
}

/// Draw a bordered box for every visible pane in the active tab, coloring the
/// focused pane's border (and an unfocused hovered pane's), writing its title
/// into the top border, and drawing its scroll position when it is scrolled back.
///
/// Each border takes [`compute_pane_border_style`], with every pane
/// `placement_target` names as a placement target. The top border of the pane
/// whose placement handle the pointer rests on
/// ([`is_placement_handle_hovered`]) shows the handle `⠿⠿⠿`. While
/// `viewer_chrome.is_pane_placement_visible` is `true`, the pointer colors and
/// the handle are not drawn: hovering `pane-123` does not change its border
/// color. `layout_origin` shifts each pane into the centered content rect. A
/// box that covers no row `drawn_rows` draws is not drawn. The title, the
/// handle and the scroll position are drawn only when `drawn_rows` draws
/// their row.
fn draw_panes(
    render_snapshot: &RenderSnapshot,
    theme: &Theme,
    viewer_chrome: ViewerChrome,
    placement_target: Option<&PanePlacementTarget>,
    layout_origin: Point,
    drawn_rows: DrawnRows<'_>,
    screen_buffer: &mut Buffer,
) {
    let displayed_tab_id = render_snapshot.client_snapshot.active_tab_id;
    for pane_slot in &render_snapshot
        .session_snapshot
        .active_tab_snapshot
        .pane_slots
    {
        let pane_rect = place_cell_rect(pane_slot.outer_rect, layout_origin);
        if !pane_slot.is_visible || !drawn_rows.is_area_drawn(pane_rect) {
            continue;
        }
        let is_placement_target =
            is_placement_target_pane(pane_slot.pane_id, displayed_tab_id, placement_target);
        let border_style = compute_pane_border_style(
            render_snapshot,
            pane_slot.pane_id,
            is_placement_target,
            viewer_chrome,
            theme,
        );
        draw_pane_border(pane_rect, border_style, drawn_rows, screen_buffer);

        let pane_snapshot = render_snapshot.find_pane_snapshot(pane_slot.pane_id);

        // The pane label starts two cells in and stops four cells short of the
        // box width, which leaves the corner glyphs.
        let pane_label = pane_snapshot
            .and_then(|pane_snapshot| pane_snapshot.pane_title.as_deref())
            .unwrap_or("");
        let is_top_border_drawn = drawn_rows.is_row_drawn(pane_rect.y);
        if !pane_label.is_empty() && pane_rect.width > 4 && is_top_border_drawn {
            let pane_label_line = Line::from(Span::styled(format!(" {pane_label} "), border_style));
            set_line_clipped(
                screen_buffer,
                pane_rect.x + 2,
                pane_rect.y,
                &pane_label_line,
                pane_rect.width - 4,
            );
        }

        if is_top_border_drawn && is_placement_handle_hovered(pane_slot.pane_id, viewer_chrome) {
            if let Some(handle_rect) = compute_placement_handle_rect(pane_slot.outer_rect) {
                let handle_origin = place_cell_rect(handle_rect, layout_origin);
                let handle_text = "⠿".repeat(usize::from(PLACEMENT_HANDLE_COLUMN_COUNT));
                screen_buffer.set_string(
                    handle_origin.x,
                    handle_origin.y,
                    &handle_text,
                    border_style,
                );
            }
        }

        if let Some(pane_snapshot) =
            pane_snapshot.filter(|_| drawn_rows.is_row_drawn(pane_rect.bottom().saturating_sub(1)))
        {
            draw_pane_scroll_position(pane_snapshot, pane_rect, border_style, screen_buffer);
        }
    }
}

/// Draw the one-cell border of `pane_area`, cut to the buffer, in
/// `border_style` on each row `drawn_rows` draws: `┌─┐` on the top row,
/// `└─┘` on the bottom row and `│` at both ends of each row between. The
/// cells inside the border keep their symbols and styles. A one-row area
/// draws `┌─┐`, and a one-column area draws `┌`, then `│`, then `└`.
/// Example: a `4x3` area draws `┌──┐`, `│  │` and `└──┘`.
fn draw_pane_border(
    pane_area: RatatuiRect,
    border_style: Style,
    drawn_rows: DrawnRows<'_>,
    screen_buffer: &mut Buffer,
) {
    let border_area = pane_area.intersection(screen_buffer.area);
    if border_area.is_empty() {
        return;
    }
    let left_column = border_area.left();
    let right_column = border_area.right() - 1;
    let top_row = border_area.top();
    let bottom_row = border_area.bottom() - 1;
    for row_index in drawn_rows.list_rows_between(border_area.top(), border_area.bottom()) {
        let (left_symbol, inner_symbol, right_symbol) = if row_index == top_row {
            ("┌", Some("─"), "┐")
        } else if row_index == bottom_row {
            ("└", Some("─"), "┘")
        } else {
            ("│", None, "│")
        };
        if let Some(inner_symbol) = inner_symbol {
            for column_index in left_column + 1..right_column {
                screen_buffer[(column_index, row_index)]
                    .set_symbol(inner_symbol)
                    .set_style(border_style);
            }
        }
        screen_buffer[(right_column, row_index)]
            .set_symbol(right_symbol)
            .set_style(border_style);
        screen_buffer[(left_column, row_index)]
            .set_symbol(left_symbol)
            .set_style(border_style);
    }
}

/// The style of the border of the pane `pane_id`, and of the text drawn in
/// that border. The first match wins:
///
/// 1. The focus style, when `pane_id` is `viewer_chrome.placement_source_pane_id`,
///    or, with no placement source, the client's `focused_pane_id`.
/// 2. The placement hover style, when `is_placement_target` is `true` or the
///    pointer rests on the pane's placement handle ([`is_placement_handle_hovered`]).
/// 3. The hover style, when `viewer_chrome.hovered_pane_id` is `pane_id` and
///    `viewer_chrome.is_pane_placement_visible` is `false`.
/// 4. The unfocused style.
fn compute_pane_border_style(
    render_snapshot: &RenderSnapshot,
    pane_id: PaneId,
    is_placement_target: bool,
    viewer_chrome: ViewerChrome,
    theme: &Theme,
) -> Style {
    let focused_pane_id = viewer_chrome
        .placement_source_pane_id
        .or(render_snapshot.client_snapshot.focused_pane_id);
    if Some(pane_id) == focused_pane_id {
        compute_focused_border_style(theme)
    } else if is_placement_target || is_placement_handle_hovered(pane_id, viewer_chrome) {
        compute_placement_hover_border_style(theme)
    } else if !viewer_chrome.is_pane_placement_visible
        && Some(pane_id) == viewer_chrome.hovered_pane_id
    {
        compute_hover_border_style(theme)
    } else {
        compute_unfocused_border_style(theme)
    }
}

/// Whether the viewer's pointer rests on the placement handle of `pane_id`:
/// `viewer_chrome.placement_handle_pane_id` is `pane_id` and
/// `viewer_chrome.is_pane_placement_visible` is `false`.
fn is_placement_handle_hovered(pane_id: PaneId, viewer_chrome: ViewerChrome) -> bool {
    !viewer_chrome.is_pane_placement_visible
        && Some(pane_id) == viewer_chrome.placement_handle_pane_id
}

/// Write the scroll position of a pane scrolled back into the bottom border of
/// its box `pane_rect`, right-aligned against the bottom-right corner: a pane
/// scrolled 5 lines up of 200 retained shows ` 5/200 `. Writes nothing for a
/// pane at the live tail, or when `pane_rect` is narrower than the text plus
/// its two corners.
fn draw_pane_scroll_position(
    pane_snapshot: &PaneSnapshot,
    pane_rect: RatatuiRect,
    border_style: Style,
    screen_buffer: &mut Buffer,
) {
    let Some((scrolled_line_count, retained_line_count)) = get_pane_scroll(pane_snapshot) else {
        return;
    };
    let scroll_text = format!(" {scrolled_line_count}/{retained_line_count} ");
    let scroll_text_width = get_text_width(&scroll_text);
    if pane_rect.width < scroll_text_width + 2 {
        return;
    }
    let scroll_line = Line::from(Span::styled(scroll_text, border_style));
    let scroll_start_column = pane_rect.right() - 1 - scroll_text_width;
    set_line_clipped(
        screen_buffer,
        scroll_start_column,
        pane_rect.bottom() - 1,
        &scroll_line,
        scroll_text_width,
    );
}

/// Return whether `placement_target` names `pane_id` in the tab `displayed_tab_id`.
///
/// A swap names its target pane in every tab. An insertion names panes only in
/// its destination tab: its anchor pane, every pane of its anchor group, or
/// every pane for a whole-tab anchor. `None` names no pane.
#[must_use]
pub fn is_placement_target_pane(
    pane_id: PaneId,
    displayed_tab_id: TabId,
    placement_target: Option<&PanePlacementTarget>,
) -> bool {
    match placement_target {
        Some(PanePlacementTarget::Swap { target_pane_id }) => pane_id == *target_pane_id,
        Some(PanePlacementTarget::Split {
            destination_tab_id,
            anchor,
            ..
        }) if *destination_tab_id == displayed_tab_id => match anchor {
            PanePlacementAnchor::Pane(target_pane_id) => pane_id == *target_pane_id,
            PanePlacementAnchor::Group(target_pane_ids) => target_pane_ids.contains(&pane_id),
            PanePlacementAnchor::Tab => true,
        },
        Some(PanePlacementTarget::Split { .. }) | None => false,
    }
}

/// Tint the content of the pane whose placement handle the pointer rests on
/// ([`is_placement_handle_hovered`]), on each row `drawn_rows` draws. Nothing
/// is tinted while `viewer_chrome.is_pane_placement_visible` is `true`.
fn draw_placement_hover_tints(
    render_snapshot: &RenderSnapshot,
    viewer_chrome: ViewerChrome,
    layout_origin: Point,
    drawn_rows: DrawnRows<'_>,
    screen_buffer: &mut Buffer,
) {
    for pane_slot in &render_snapshot
        .session_snapshot
        .active_tab_snapshot
        .pane_slots
    {
        if !pane_slot.is_visible || !is_placement_handle_hovered(pane_slot.pane_id, viewer_chrome) {
            continue;
        }
        let Some(content_rect) = pane_slot.content_rect else {
            continue;
        };
        draw_placement_hover_tint(
            place_cell_rect(content_rect, layout_origin),
            drawn_rows,
            screen_buffer,
        );
    }
}

/// Set the background of every cell of `content_area` inside the buffer, on
/// each row `drawn_rows` draws, to the placement hover tint, keeping each
/// cell's text and foreground.
fn draw_placement_hover_tint(
    content_area: RatatuiRect,
    drawn_rows: DrawnRows<'_>,
    screen_buffer: &mut Buffer,
) {
    let content_area = content_area.intersection(screen_buffer.area);
    for row_index in drawn_rows.list_rows_between(content_area.top(), content_area.bottom()) {
        for column_index in content_area.left()..content_area.right() {
            screen_buffer[(column_index, row_index)].set_bg(PLACEMENT_HOVER_TINT_COLOR);
        }
    }
}

/// Draw the "terminal too small" overlay: one centered, bold line telling the
/// user to enlarge the window, shown when the tab has no room for any pane.
///
/// Centered on the middle row of `viewport_area` and horizontally within it. A
/// message wider than `viewport_area` is clipped at its right edge.
fn draw_too_small_overlay(viewport_area: RatatuiRect, screen_buffer: &mut Buffer) {
    let too_small_message = Line::from(Span::styled(
        "Terminal too small — enlarge window",
        compute_too_small_overlay_style(),
    ));
    let too_small_message_width = get_line_width(&too_small_message);
    let too_small_message_start_column =
        viewport_area.x + viewport_area.width.saturating_sub(too_small_message_width) / 2;
    let message_row = viewport_area.y + viewport_area.height / 2;
    set_line_clipped(
        screen_buffer,
        too_small_message_start_column,
        message_row,
        &too_small_message,
        viewport_area
            .right()
            .saturating_sub(too_small_message_start_column),
    );
}

/// Paint each visible terminal pane's cells into its content rect.
///
/// For every visible pane slot that has a content rect and a terminal grid,
/// draws the grid into that rect. Panes with no grid or no content rect
/// (suppressed, hidden, or a collapsed stack member) draw nothing.
/// `layout_origin` shifts each content rect into the centered content area.
/// Only the rows `drawn_rows` draws are drawn.
fn draw_pane_contents(
    render_snapshot: &RenderSnapshot,
    layout_origin: Point,
    drawn_rows: DrawnRows<'_>,
    screen_buffer: &mut Buffer,
) {
    for (pane_snapshot, placed_content_rect) in
        list_visible_tiled_panes(render_snapshot, layout_origin)
    {
        let Some(terminal_grid_view) = &pane_snapshot.terminal_grid_view else {
            continue;
        };
        draw_grid(
            terminal_grid_view,
            placed_content_rect,
            pane_snapshot.is_reverse_video,
            pane_snapshot.selection_spans.as_ref(),
            drawn_rows,
            screen_buffer,
        );
    }
}

/// Paint one pane's `grid_view` into `target_area`, one buffer cell per grid cell.
///
/// Each grid cell is placed at its own column, so on-screen column positions
/// always match grid column positions. The continuation half of a
/// wide glyph (width 0) is skipped — the wide base already covers it. A wide
/// glyph whose second half falls outside the content area is replaced by a blank
/// so no half-glyph bleeds past the edge. A cell whose base character and
/// combining marks together take more columns than the cell
/// ([`compute_symbol_display_width`]) draws its base character alone: `U+2764`
/// with VS16 in a 1-column cell draws `U+2764`. `is_reverse_video` (DECSCNM) toggles
/// reverse for every cell. `target_area` is clipped to the buffer so an oversized rect
/// cannot index out of bounds.
///
/// A highlighted cell in `selection_spans` is drawn in reverse. The highlight
/// combines with the cell's own reverse and with `is_reverse_video` by exclusive-or, so
/// highlighting a cell that is already reverse swaps it back to normal.
///
/// Only the screen rows `drawn_rows` draws are drawn. A run of cells with one
/// style and one reverse state converts the style once, and the symbol of
/// each cell with combining marks is built in one string that the call
/// reuses.
fn draw_grid(
    grid_view: &GridView,
    target_area: RatatuiRect,
    is_reverse_video: bool,
    selection_spans: Option<&SelectionSpans>,
    drawn_rows: DrawnRows<'_>,
    screen_buffer: &mut Buffer,
) {
    let clipped_area = target_area.intersection(screen_buffer.area);
    let (grid_row_count, grid_column_count) = grid_view.get_view_dimensions();
    let visible_row_count = grid_row_count.min(clipped_area.height);
    let visible_column_count = grid_column_count.min(clipped_area.width);
    let mut converted_style: Option<(CellStyle, bool, Style)> = None;
    let mut cell_symbol = String::new();
    for screen_row in
        drawn_rows.list_rows_between(clipped_area.y, clipped_area.y + visible_row_count)
    {
        let row_index = screen_row - clipped_area.y;
        let Some(row_snapshot) = grid_view.row_snapshots.get(usize::from(row_index)) else {
            continue;
        };
        let cell_row = row_snapshot.list_row_cells();
        // A highlight is one column range per row, resolved before the row's
        // cells are walked.
        let row_span = selection_spans.and_then(|spans| spans.find_row_span(row_index));
        for column_index in 0..visible_column_count {
            let Some(cell) = cell_row.get(usize::from(column_index)) else {
                continue;
            };
            let cell_width = cell.get_display_width();
            if cell_width == 0 {
                continue;
            }
            let screen_column = clipped_area.x + column_index;
            let is_selected = row_span.is_some_and(|(start_column, end_column)| {
                column_index >= start_column && column_index <= end_column
            });
            let is_cell_reversed = is_reverse_video ^ is_selected;
            let cell_style = match converted_style {
                Some((last_cell_style, last_is_cell_reversed, last_style))
                    if last_cell_style == cell.get_style()
                        && last_is_cell_reversed == is_cell_reversed =>
                {
                    last_style
                }
                _ => {
                    let cell_style = get_cell_style(cell.get_style(), is_cell_reversed);
                    converted_style = Some((cell.get_style(), is_cell_reversed, cell_style));
                    cell_style
                }
            };
            if cell_width >= 2 && column_index + 1 >= visible_column_count {
                screen_buffer[(screen_column, screen_row)]
                    .set_char(' ')
                    .set_style(cell_style);
                continue;
            }
            let screen_cell = &mut screen_buffer[(screen_column, screen_row)];
            screen_cell.set_style(cell_style);
            if cell.list_combining_characters().is_empty() {
                screen_cell.set_char(cell.get_character());
                continue;
            }
            cell_symbol.clear();
            cell_symbol.push(cell.get_character());
            cell_symbol.extend(cell.list_combining_characters().iter().copied());
            if compute_symbol_display_width(&cell_symbol) > u16::from(cell_width) {
                screen_cell.set_char(cell.get_character());
            } else {
                screen_cell.set_symbol(&cell_symbol);
            }
        }
    }
}

/// Give each cell of `viewport_area` whose symbol ratatui measures at another
/// width ([`CellWidth::cell_width`]) than [`compute_symbol_display_width`]
/// that width as its forced width ([`CellDiffOption::ForcedWidth`]). A cell
/// whose symbol takes 0 columns keeps ratatui's width.
///
/// Example: ratatui measures `U+17D8` 3 columns wide, and
/// [`compute_symbol_display_width`] gives it 1. Its cell gets the forced width
/// 1, and the frame diff takes the cell after it as the next screen column.
/// Only the rows `drawn_rows` draws are visited.
fn apply_display_widths(
    viewport_area: RatatuiRect,
    drawn_rows: DrawnRows<'_>,
    screen_buffer: &mut Buffer,
) {
    let drawn_area = viewport_area.intersection(screen_buffer.area);
    for row_index in drawn_rows.list_rows_between(drawn_area.top(), drawn_area.bottom()) {
        for column_index in drawn_area.left()..drawn_area.right() {
            let screen_cell = &mut screen_buffer[(column_index, row_index)];
            let display_width = compute_symbol_display_width(screen_cell.symbol());
            if screen_cell.cell_width() == display_width {
                continue;
            }
            if let Some(forced_cell_width) = NonZeroU16::new(display_width) {
                screen_cell.set_diff_option(CellDiffOption::ForcedWidth(forced_cell_width));
            }
        }
    }
}

/// The columns `symbol`, the text of one cell or one grapheme, takes on the
/// screen: its [`compute_text_display_width`], at most `u16::MAX`. Example:
/// `U+17D8` is 1, `世` is 2, and `世ﾞ` (`U+4E16` `U+FF9E`) is 3.
fn compute_symbol_display_width(symbol: &str) -> u16 {
    u16::try_from(compute_text_display_width(symbol)).unwrap_or(u16::MAX)
}

/// Map a terminal cell style to a ratatui [`Style`].
///
/// Colors map directly, the terminal default becoming ratatui's reset. Each
/// boolean attribute maps to its modifier; every underline variant collapses to
/// a single underline, and overline and underline color have no ratatui modifier
/// and are not drawn. `is_reverse_video` (DECSCNM) combines with the cell's own
/// reverse by exclusive-or, so a screen-wide reverse cancels a cell already in
/// reverse.
fn get_cell_style(cell_style: CellStyle, is_reverse_video: bool) -> Style {
    let cell_attributes = cell_style.get_attributes();
    let mut text_modifier = Modifier::empty();
    if cell_attributes.is_bold() {
        text_modifier |= Modifier::BOLD;
    }
    if cell_attributes.is_faint() {
        text_modifier |= Modifier::DIM;
    }
    if cell_attributes.is_italic() {
        text_modifier |= Modifier::ITALIC;
    }
    if cell_attributes.get_underline_style() != UnderlineStyle::None {
        text_modifier |= Modifier::UNDERLINED;
    }
    if cell_attributes.is_blinking() {
        text_modifier |= Modifier::SLOW_BLINK;
    }
    if cell_attributes.is_concealed() {
        text_modifier |= Modifier::HIDDEN;
    }
    if cell_attributes.is_strikethrough() {
        text_modifier |= Modifier::CROSSED_OUT;
    }
    if cell_attributes.is_reverse() ^ is_reverse_video {
        text_modifier |= Modifier::REVERSED;
    }
    Style::default()
        .fg(get_cell_color(cell_style.get_foreground_color()))
        .bg(get_cell_color(cell_style.get_background_color()))
        .add_modifier(text_modifier)
}

/// Map a terminal color to a ratatui [`Color`]; the terminal default becomes
/// ratatui's reset (the outer terminal's own default).
fn get_cell_color(cell_color: CellColor) -> Color {
    match cell_color {
        CellColor::Default => Color::Reset,
        CellColor::Indexed(color_index) => Color::Indexed(color_index),
        CellColor::Rgb(red_channel, green_channel, blue_channel) => {
            Color::Rgb(red_channel, green_channel, blue_channel)
        }
    }
}

/// Draw one header strip for each collapsed stack member: a collapse arrow and
/// the pane title on the left, and a `[position/total]` indicator on the right.
/// The strip of `viewer_chrome.placement_source_pane_id` takes the focus color, and
/// a strip that `placement_target` names takes the placement hover color.
/// `layout_origin` shifts each strip into the centered content rect. A strip
/// on no row `drawn_rows` draws is not drawn.
fn draw_stack_headers(
    render_snapshot: &RenderSnapshot,
    theme: &Theme,
    viewer_chrome: ViewerChrome,
    placement_target: Option<&PanePlacementTarget>,
    layout_origin: Point,
    drawn_rows: DrawnRows<'_>,
    screen_buffer: &mut Buffer,
) {
    let displayed_tab_id = render_snapshot.client_snapshot.active_tab_id;
    for stack_header in &render_snapshot
        .session_snapshot
        .active_tab_snapshot
        .stack_headers
    {
        let header_rect = place_cell_rect(stack_header.header_rect, layout_origin);
        if header_rect.width == 0
            || header_rect.height == 0
            || !drawn_rows.is_area_drawn(header_rect)
        {
            continue;
        }

        let stack_header_style = if Some(stack_header.pane_id)
            == viewer_chrome.placement_source_pane_id
        {
            compute_focused_border_style(theme)
        } else if is_placement_target_pane(stack_header.pane_id, displayed_tab_id, placement_target)
        {
            compute_placement_hover_border_style(theme)
        } else {
            compute_stack_header_style(theme)
        };

        // Fill the whole row first: the gap between the pane label and indicator
        // carries the strip background too.
        screen_buffer.set_style(header_rect, stack_header_style);

        let pane_label = render_snapshot
            .find_pane_snapshot(stack_header.pane_id)
            .and_then(|pane_snapshot| pane_snapshot.pane_title.as_deref())
            .unwrap_or("");
        let pane_label_line = Line::from(format!("▸ {pane_label}"));
        set_line_clipped(
            screen_buffer,
            header_rect.x,
            header_rect.y,
            &pane_label_line,
            header_rect.width,
        );

        // Right-align `[N/total]`, with its start column clamped to
        // `header_rect.x`. A strip narrower than the indicator writes only inside
        // the strip.
        let indicator_line = Line::from(format!(
            "[{}/{}]",
            stack_header.member_index + 1,
            stack_header.member_count
        ));
        let indicator_width = get_line_width(&indicator_line);
        let indicator_start_column = header_rect
            .right()
            .saturating_sub(indicator_width)
            .max(header_rect.x);
        set_line_clipped(
            screen_buffer,
            indicator_start_column,
            header_rect.y,
            &indicator_line,
            header_rect.right() - indicator_start_column,
        );
    }
}

/// The mode indicator shown in the tabline: every active mode label joined with
/// ` · `, or `BASE` when the client is in plain mode with the mouse ungrabbed
/// and `reconnecting` is `None`.
///
/// The labels compose from independent axes, always in this order:
/// `reconnecting` adds `RECONNECTING (attempt 4, retry in 8s)` from a
/// `Reconnecting { attempt: 4, retry_in_seconds: Some(8) }` and
/// `RECONNECTING (attempt 4, dialing)` from a
/// `Reconnecting { attempt: 4, retry_in_seconds: None }`, the `lock_mode` layer
/// contributes at most one tag (nothing when `Normal`), and mouse-selection mode adds
/// `SELECT`. So that same client, locked and grabbing the mouse, reads
/// `RECONNECTING (attempt 4, retry in 8s) · LOCK · SELECT`, and a plain one
/// grabbing it reads `SELECT`. A client with `reconnecting` set never reads
/// `BASE`.
fn build_mode_tags(
    lock_mode: LockMode,
    is_mouse_selection_enabled: bool,
    reconnecting: Option<Reconnecting>,
) -> String {
    let reconnect_tag =
        reconnecting.map(
            |reconnecting_state| match reconnecting_state.retry_in_seconds {
                Some(retry_in_seconds) => format!(
                    "RECONNECTING (attempt {}, retry in {retry_in_seconds}s)",
                    reconnecting_state.attempt
                ),
                None => format!(
                    "RECONNECTING (attempt {}, dialing)",
                    reconnecting_state.attempt
                ),
            },
        );
    let mut mode_tag_labels: Vec<&str> = Vec::new();
    if let Some(mode_tag) = reconnect_tag.as_deref() {
        mode_tag_labels.push(mode_tag);
    }
    if let Some(mode_tag) = lock_mode_tag(lock_mode) {
        mode_tag_labels.push(mode_tag);
    }
    if is_mouse_selection_enabled {
        mode_tag_labels.push("SELECT");
    }
    if mode_tag_labels.is_empty() {
        "BASE".to_string()
    } else {
        mode_tag_labels.join(" · ")
    }
}

/// The tag for a non-plain lock mode, or `None` for `Normal` — which shows as
/// `BASE` only when no other mode is active.
fn lock_mode_tag(lock_mode: LockMode) -> Option<&'static str> {
    match lock_mode {
        LockMode::Normal => None,
        LockMode::Locked => Some("LOCK"),
        LockMode::Resize => Some("RESIZE"),
        LockMode::PanePlacement => Some("PLACE PANE"),
        LockMode::TabMode => Some("TAB"),
        LockMode::ScrollMode => Some("SCROLL"),
        LockMode::PaneMoveChoice => Some("MOVE"),
        LockMode::FloatingPaneMove => Some("MOVE FLOAT"),
    }
}

/// A pane's scroll position as `(lines scrolled up, retained lines)`, or `None`
/// when the pane is at the live tail (nothing to indicate).
fn get_pane_scroll(pane_snapshot: &PaneSnapshot) -> Option<(usize, usize)> {
    let view_row_offset = pane_snapshot
        .terminal_grid_view
        .as_ref()
        .map_or(0, |grid_view| grid_view.view_row_offset);
    (view_row_offset > 0).then_some((
        view_row_offset,
        pane_snapshot.scrollback_metadata.retained_line_count,
    ))
}

/// The top-left cell of a ratatui rect, as the layout origin [`place_cell_rect`]
/// shifts by. An
/// area at `column: 20, row: 1` gives `Point { column: 20, row: 1 }`.
fn get_area_origin(screen_area: RatatuiRect) -> Point {
    Point {
        column: screen_area.x,
        row: screen_area.y,
    }
}

/// Place a koshi-core cell rect into a ratatui area by shifting its origin.
///
/// A region solution starts at `(0, 0)`; a ratatui frame area can start elsewhere.
pub(crate) fn place_cell_rect(cell_rect: Rect, layout_origin: Point) -> RatatuiRect {
    RatatuiRect {
        x: cell_rect.origin.column + layout_origin.column,
        y: cell_rect.origin.row + layout_origin.row,
        width: cell_rect.size.column_count,
        height: cell_rect.size.row_count,
    }
}

/// Each visible tiled pane of the active tab that has a content rect and a pane
/// snapshot, in pane slot order: its snapshot, and its content rect placed at
/// `layout_origin` by [`place_cell_rect`]. A hidden pane, a pane with no
/// content rect, and a pane with no snapshot are left out.
pub(crate) fn list_visible_tiled_panes(
    render_snapshot: &RenderSnapshot,
    layout_origin: Point,
) -> impl Iterator<Item = (&PaneSnapshot, RatatuiRect)> + '_ {
    render_snapshot
        .session_snapshot
        .active_tab_snapshot
        .pane_slots
        .iter()
        .filter(|pane_slot| pane_slot.is_visible)
        .filter_map(move |pane_slot| {
            let content_rect = pane_slot.content_rect?;
            let pane_snapshot = render_snapshot.find_pane_snapshot(pane_slot.pane_id)?;
            Some((pane_snapshot, place_cell_rect(content_rect, layout_origin)))
        })
}

/// Return one committed region in the ratatui frame's coordinate space.
pub(crate) fn find_region_area(
    committed_regions: &CommittedRegions,
    region_index: usize,
    viewport_area: RatatuiRect,
) -> Option<RatatuiRect> {
    committed_regions
        .solved_regions
        .region_rects
        .get(region_index)
        .map(|&region_rect| place_cell_rect(region_rect, get_area_origin(viewport_area)))
}

/// The buffer cell where the active tab's solved layout starts: the top-left of
/// the layout's `tab_size` centered in the pane area that
/// `committed_regions` leave inside `viewport_area`.
pub(crate) fn compute_layout_origin(
    render_snapshot: &RenderSnapshot,
    committed_regions: &CommittedRegions,
    viewport_area: RatatuiRect,
) -> Point {
    get_area_origin(compute_content_rect(
        compute_pane_area(committed_regions, viewport_area),
        render_snapshot
            .session_snapshot
            .active_tab_snapshot
            .tab_size,
    ))
}

/// Return the pane rectangle left by the committed regions in frame coordinates.
pub fn compute_pane_area(
    committed_regions: &CommittedRegions,
    viewport_area: RatatuiRect,
) -> RatatuiRect {
    place_cell_rect(
        committed_regions.solved_regions.pane_rect,
        get_area_origin(viewport_area),
    )
}

/// The cells `text` occupies when drawn ([`count_span_cells`]), never counted
/// from its bytes or chars: `漢字` is 4, `🦀` is 2, `e` plus a combining acute
/// is 1, `ｶﾞ` (`U+FF76` `U+FF9E`) is 2, and `U+17D8` is 1.
///
/// Text wider than `u16::MAX` cells gives `u16::MAX`, which every width
/// comparison in this crate reads as wider than the row.
pub(crate) fn get_text_width(text: &str) -> u16 {
    u16::try_from(count_span_cells(&Span::raw(text))).unwrap_or(u16::MAX)
}

/// The cells `line` occupies when drawn: [`count_span_cells`] of each span,
/// summed, and never taken from their styles.
///
/// A line wider than `u16::MAX` cells gives `u16::MAX`, which every width
/// comparison in this crate reads as wider than the row.
pub(crate) fn get_line_width(line: &Line<'_>) -> u16 {
    let line_cell_count: usize = line.spans.iter().map(count_span_cells).sum();
    u16::try_from(line_cell_count).unwrap_or(u16::MAX)
}

/// The cells [`set_line_clipped`] places `span` in: the
/// [`compute_text_display_width`] of each grapheme of the span that holds no
/// control character, summed. Example: `ｶﾞ` (`U+FF76` `U+FF9E`) is 2, and
/// `U+17D8` is 1.
fn count_span_cells(span: &Span<'_>) -> usize {
    span.styled_graphemes(Style::default())
        .map(|styled_grapheme| compute_text_display_width(styled_grapheme.symbol))
        .sum()
}

/// Draw `line` from `start_column` on `row_index`, in at most `maximum_width`
/// columns and never past the buffer's right edge. A `row_index` outside the
/// buffer's rows writes nothing: a buffer shorter than the laid-out frame
/// leaves the chrome rows below it undrawn.
///
/// Each span is drawn in the line's style patched with the span's own. Each
/// grapheme with no control character takes the columns that
/// [`compute_symbol_display_width`] gives it. Its first cell gets the grapheme
/// and the style, and each further cell it covers is reset. A grapheme of 0
/// columns is left out. A span stops at its first grapheme that does not fit
/// the columns left, and the next span is drawn from the column where that one
/// stopped.
///
/// Example: `U+17D8` then `x` in 2 columns puts `U+17D8` at `start_column` and
/// `x` in the column after it. `漢x` in 1 column draws nothing.
pub(crate) fn set_line_clipped(
    screen_buffer: &mut Buffer,
    start_column: u16,
    row_index: u16,
    line: &Line<'_>,
    maximum_width: u16,
) {
    if row_index < screen_buffer.area.top() || row_index >= screen_buffer.area.bottom() {
        return;
    }
    let end_column = screen_buffer
        .area
        .right()
        .min(start_column.saturating_add(maximum_width));
    let mut column_index = start_column;
    for span in &line.spans {
        for styled_grapheme in span.styled_graphemes(line.style) {
            let grapheme_width = compute_symbol_display_width(styled_grapheme.symbol);
            if grapheme_width == 0 {
                continue;
            }
            let Some(next_column_index) = column_index
                .checked_add(grapheme_width)
                .filter(|next_column_index| *next_column_index <= end_column)
            else {
                break;
            };
            screen_buffer[(column_index, row_index)]
                .set_symbol(styled_grapheme.symbol)
                .set_style(styled_grapheme.style);
            for covered_column_index in column_index + 1..next_column_index {
                screen_buffer[(covered_column_index, row_index)].reset();
            }
            column_index = next_column_index;
        }
    }
}

/// The centered rect of the effective (solved) size within the pane area.
///
/// A pane area larger than `tab_size` leaves a letterbox margin around the
/// rect. Each dimension is clamped to the pane area's own.
pub fn compute_content_rect(pane_area: RatatuiRect, tab_size: Size) -> RatatuiRect {
    let column_count = tab_size.column_count.min(pane_area.width);
    let row_count = tab_size.row_count.min(pane_area.height);
    RatatuiRect {
        x: pane_area.x + (pane_area.width - column_count) / 2,
        y: pane_area.y + (pane_area.height - row_count) / 2,
        width: column_count,
        height: row_count,
    }
}

/// Fill the letterbox margin — the cells of `viewport_area` outside the centered
/// `effective_layout_rect` — with a dim backdrop. Does nothing when the content fills the
/// whole area.
///
/// The margin is the four bands around `effective_layout_rect` once it is cut
/// to `viewport_area`: a layout solved for a larger viewport reaches past the
/// painted area, and the part outside it is not a band. Each band is restyled
/// in place, never blanked: [`render_frame`] clears every cell of each drawn
/// row of `viewport_area` before this runs. [`Buffer::set_style`] clips to the
/// buffer, so a viewport area larger than `screen_buffer` writes only inside
/// `screen_buffer`. Only the rows `drawn_rows` draws are restyled.
fn draw_letterbox(
    viewport_area: RatatuiRect,
    effective_layout_rect: RatatuiRect,
    theme: &Theme,
    drawn_rows: DrawnRows<'_>,
    screen_buffer: &mut Buffer,
) {
    let effective_layout_rect = viewport_area.intersection(effective_layout_rect);
    if effective_layout_rect == viewport_area {
        return;
    }
    let letterbox_style = compute_letterbox_style(theme);
    let letterbox_bands = [
        // Above the content, full width.
        RatatuiRect {
            x: viewport_area.x,
            y: viewport_area.y,
            width: viewport_area.width,
            height: effective_layout_rect.y - viewport_area.y,
        },
        // Below the content, full width.
        RatatuiRect {
            x: viewport_area.x,
            y: effective_layout_rect.bottom(),
            width: viewport_area.width,
            height: viewport_area
                .bottom()
                .saturating_sub(effective_layout_rect.bottom()),
        },
        // Left of the content, its own height.
        RatatuiRect {
            x: viewport_area.x,
            y: effective_layout_rect.y,
            width: effective_layout_rect.x - viewport_area.x,
            height: effective_layout_rect.height,
        },
        // Right of the content, its own height.
        RatatuiRect {
            x: effective_layout_rect.right(),
            y: effective_layout_rect.y,
            width: viewport_area
                .right()
                .saturating_sub(effective_layout_rect.right()),
            height: effective_layout_rect.height,
        },
    ];
    for letterbox_band in letterbox_bands {
        for row_index in drawn_rows.list_rows_between(letterbox_band.top(), letterbox_band.bottom())
        {
            screen_buffer.set_style(
                RatatuiRect {
                    y: row_index,
                    height: 1,
                    ..letterbox_band
                },
                letterbox_style,
            );
        }
    }
}

mod style;
mod tabline;

use style::*;
use tabline::draw_tabline;
// The statusline fills its row with the same bar background as the tabline.
pub(crate) use style::compute_bar_style;
pub(crate) use tabline::solve_tabline_layout;

#[cfg(test)]
pub(crate) mod tests;
