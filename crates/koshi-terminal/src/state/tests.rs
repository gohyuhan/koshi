//! Unit tests for per-pane terminal state.

use super::*;
use crate::grid::state::{Cell, Grid, RowEnd};
use crate::style::{Color, Style};

/// Overwrite the cell at (`row`, `col`) of the active grid with `ch` of the
/// given display `width`, in the default style — used to plant wide glyphs
/// (base `width == 2` + a `width == 0` continuation) for the `clip_row` tests.
fn put(state: &mut TerminalState, row: u16, col: u16, ch: char, width: u8) {
    *state.active_grid_mut().cell_mut(row, col).unwrap() = Cell::new(ch, width, Style::default());
}

#[test]
fn new_initializes_both_screens_to_blank_of_size() {
    let state = TerminalState::new(PtySize { cols: 5, rows: 3 });
    assert_eq!(*state.primary, Grid::blank(3, 5, Style::default()));
    assert_eq!(*state.alternate, Grid::blank(3, 5, Style::default()));
}

#[test]
fn new_starts_on_primary_with_default_cursor_style_and_no_title() {
    let state = TerminalState::new(PtySize { cols: 80, rows: 24 });
    assert_eq!(state.active, Screen::Primary);
    let expected_cursor = Cursor {
        row: 0,
        col: 0,
        is_visible: true,
        pending_wrap: false,
        saved: None,
    };
    assert_eq!(state.primary_cursor, expected_cursor);
    assert_eq!(state.alternate_cursor, expected_cursor);
    assert_eq!(state.active_render().charsets, [Charset::default(); 4]);
    assert_eq!(state.active_render().gl, 0);
    assert_eq!(state.active_render().style, Style::default());
    assert_eq!(state.primary_render, state.alternate_render);
    assert_eq!(state.modes, TerminalModes::default());
    assert_eq!(state.title, None);
}

#[test]
fn active_grid_follows_active_screen() {
    let mut state = TerminalState::new(PtySize { cols: 4, rows: 2 });
    assert!(std::ptr::eq(state.active_grid(), state.primary.as_ref()));
    state.active = Screen::Alternate;
    assert!(std::ptr::eq(state.active_grid(), state.alternate.as_ref()));
}

#[test]
fn active_grid_mut_follows_active_screen() {
    let mut state = TerminalState::new(PtySize { cols: 4, rows: 2 });
    assert_eq!(
        state.active_grid_mut(),
        &Grid::blank(2, 4, Style::default())
    );
    state.active = Screen::Alternate;
    assert_eq!(
        state.active_grid_mut(),
        &Grid::blank(2, 4, Style::default())
    );
}

#[test]
fn resize_reallocs_both_grids_to_new_size() {
    let mut state = TerminalState::new(PtySize { cols: 80, rows: 24 });
    state.resize(PtySize { cols: 10, rows: 5 });
    assert_eq!(*state.primary, Grid::blank(5, 10, Style::default()));
    assert_eq!(*state.alternate, Grid::blank(5, 10, Style::default()));
}

#[test]
fn resize_pads_each_grid_with_its_own_screen_background() {
    // Padding a resize creates is filled with that screen's own render
    // background, never the other screen's. On the reflowed primary,
    // fully-default blanks count as padding, so they re-fill too — the same
    // background-color-erase fill every erase and scroll uses. Content cells
    // (anything non-default) keep their own styles.
    let mut state = TerminalState::new(PtySize { cols: 4, rows: 2 });
    put(&mut state, 0, 0, 'x', 1);
    state.primary_render.style.set_bg(Color::Indexed(4)); // primary: blue
    state.alternate_render.style.set_bg(Color::Indexed(1)); // alternate: red
    state.resize(PtySize { cols: 6, rows: 3 });

    let mut blue_fill = Style::default();
    blue_fill.set_bg(Color::Indexed(4)); // bg-only: fg + attrs stay default
    let mut red_fill = Style::default();
    red_fill.set_bg(Color::Indexed(1));

    // Content keeps its own style.
    assert_eq!(
        state.primary.cell(0, 0),
        Some(&Cell::new('x', 1, Style::default()))
    );
    // Primary padding — re-created row tails and the new bottom row — takes
    // the primary fill.
    assert_eq!(state.primary.cell(0, 5), Some(&Cell::blank_with(blue_fill)));
    assert_eq!(state.primary.cell(2, 3), Some(&Cell::blank_with(blue_fill)));
    // The alternate crops in place: its untouched cells stay default and
    // only the grown region takes the alternate fill.
    assert_eq!(state.alternate.cell(0, 0), Some(&Cell::blank()));
    assert_eq!(
        state.alternate.cell(0, 5),
        Some(&Cell::blank_with(red_fill))
    );
    assert_eq!(
        state.alternate.cell(2, 3),
        Some(&Cell::blank_with(red_fill))
    );
}

#[test]
fn resize_clamps_out_of_bounds_cursor_to_last_cell() {
    let mut state = TerminalState::new(PtySize { cols: 80, rows: 24 });
    state.primary_cursor.row = 23;
    state.primary_cursor.col = 79;
    state.resize(PtySize { cols: 10, rows: 5 });
    assert_eq!(state.primary_cursor.row, 4);
    assert_eq!(state.primary_cursor.col, 9);
}

#[test]
fn resize_leaves_in_bounds_cursor_untouched() {
    let mut state = TerminalState::new(PtySize { cols: 80, rows: 24 });
    state.primary_cursor.row = 2;
    state.primary_cursor.col = 3;
    state.resize(PtySize { cols: 10, rows: 5 });
    assert_eq!(state.primary_cursor.row, 2);
    assert_eq!(state.primary_cursor.col, 3);
}

#[test]
fn resize_clears_a_pending_wrap_latched_to_the_old_edge() {
    let mut state = TerminalState::new(PtySize { cols: 80, rows: 24 });
    state.primary_cursor.pending_wrap = true;
    state.resize(PtySize { cols: 10, rows: 5 });
    assert!(!state.primary_cursor.pending_wrap);
}

#[test]
fn resize_preserves_cell_contents_across_width_and_height_changes() {
    let mut state = TerminalState::new(PtySize { cols: 6, rows: 4 });
    put(&mut state, 0, 0, 'h', 1);
    put(&mut state, 0, 1, 'i', 1);
    put(&mut state, 1, 0, '!', 1);
    state.primary_cursor.row = 1;

    // Shrink: trailing blank rows go first, the written rows stay put.
    state.resize(PtySize { cols: 4, rows: 2 });
    assert_eq!(state.primary.cell(0, 0).unwrap().ch(), 'h');
    assert_eq!(state.primary.cell(0, 1).unwrap().ch(), 'i');
    assert_eq!(state.primary.cell(1, 0).unwrap().ch(), '!');
    assert_eq!(state.scrollback.len(), 0);

    // Grow back: the content is still where it was, new space is blank.
    state.resize(PtySize { cols: 6, rows: 4 });
    assert_eq!(state.primary.cell(0, 0).unwrap().ch(), 'h');
    assert_eq!(state.primary.cell(1, 0).unwrap().ch(), '!');
    assert_eq!(state.primary.cell(3, 5), Some(&Cell::blank()));
}

#[test]
fn resize_shrink_pushes_top_rows_to_scrollback_and_grow_pulls_them_back() {
    // Every row written, cursor on the last row: nothing blank to trim, so a
    // 2-row shrink scrolls the top two rows into history.
    let mut state = TerminalState::new(PtySize { cols: 4, rows: 4 });
    for row in 0..4 {
        put(&mut state, row, 0, char::from(b'a' + row as u8), 1);
    }
    state.primary_cursor.row = 3;

    state.resize(PtySize { cols: 4, rows: 2 });
    assert_eq!(state.scrollback.len(), 2);
    assert_eq!(state.scrollback.lines()[0].0[0].ch(), 'a');
    assert_eq!(state.scrollback.lines()[1].0[0].ch(), 'b');
    assert_eq!(state.primary.cell(0, 0).unwrap().ch(), 'c');
    assert_eq!(state.primary.cell(1, 0).unwrap().ch(), 'd');
    // The cursor followed its row up.
    assert_eq!(state.primary_cursor.row, 1);

    // Growing pulls the same rows back in at the top, newest first.
    state.resize(PtySize { cols: 4, rows: 4 });
    assert_eq!(state.scrollback.len(), 0);
    assert_eq!(state.primary.cell(0, 0).unwrap().ch(), 'a');
    assert_eq!(state.primary.cell(1, 0).unwrap().ch(), 'b');
    assert_eq!(state.primary.cell(2, 0).unwrap().ch(), 'c');
    assert_eq!(state.primary.cell(3, 0).unwrap().ch(), 'd');
    assert_eq!(state.primary_cursor.row, 3);
}

#[test]
fn resize_width_shrink_wraps_a_wide_glyph_whole() {
    // 世 occupies cols 2–3; at width 3 its base would land in the last
    // column, so the reflow leaves a spacer there and wraps the glyph whole
    // onto the next row — never a dangling half.
    let mut state = TerminalState::new(PtySize { cols: 5, rows: 2 });
    put(&mut state, 0, 0, 'a', 1);
    put(&mut state, 0, 2, '世', 2);
    put(&mut state, 0, 3, ' ', 0);

    state.resize(PtySize { cols: 3, rows: 2 });
    assert_eq!(state.primary.cell(0, 0).unwrap().ch(), 'a');
    assert_eq!(state.primary.cell(0, 2), Some(&Cell::blank()));
    assert_eq!(state.primary.row_end(0), RowEnd::SoftWide);
    assert_eq!(state.primary.cell(1, 0).unwrap().ch(), '世');
    assert_eq!(state.primary.cell(1, 0).unwrap().width(), 2);
    assert_eq!(state.primary.cell(1, 1).unwrap().width(), 0);
}

#[test]
fn resize_alternate_screen_crops_without_touching_scrollback() {
    let mut state = TerminalState::new(PtySize { cols: 4, rows: 3 });
    state.active = Screen::Alternate;
    for row in 0..3 {
        put(&mut state, row, 0, char::from(b'x' + row as u8), 1);
    }

    state.resize(PtySize { cols: 4, rows: 2 });
    // The top row is cropped away — the alternate screen has no history.
    assert_eq!(state.scrollback.len(), 0);
    assert_eq!(state.alternate.cell(0, 0).unwrap().ch(), 'y');
    assert_eq!(state.alternate.cell(1, 0).unwrap().ch(), 'z');
}

#[test]
fn clip_row_passes_a_narrow_row_through_untouched() {
    let mut state = TerminalState::new(PtySize { cols: 5, rows: 1 });
    for col in 0..5 {
        put(&mut state, 0, col, 'a', 1);
    }
    let clipped = state.clip_row(0, 5);
    assert!(!clipped.right_pad());
    assert_eq!(clipped.cells().len(), 5);
}

#[test]
fn clip_row_pads_when_a_wide_base_is_the_last_visible_column() {
    // Row: a a 世 <cont> a — the wide base sits at col 2, its continuation at
    // col 3. Clipping to 3 columns ends between the halves.
    let mut state = TerminalState::new(PtySize { cols: 5, rows: 1 });
    put(&mut state, 0, 0, 'a', 1);
    put(&mut state, 0, 1, 'a', 1);
    put(&mut state, 0, 2, '世', 2);
    put(&mut state, 0, 3, ' ', 0);
    put(&mut state, 0, 4, 'a', 1);

    let clipped = state.clip_row(0, 3);
    assert!(clipped.right_pad());
    // The wide base is dropped; only the two narrow cells before it remain.
    assert_eq!(clipped.cells().len(), 2);
    assert_eq!(clipped.cells()[0].ch(), 'a');
    assert_eq!(clipped.cells()[1].ch(), 'a');
}

#[test]
fn clip_row_keeps_a_whole_wide_glyph_when_both_halves_fit() {
    // Same row, but clipping to 4 columns keeps the wide glyph's continuation.
    let mut state = TerminalState::new(PtySize { cols: 5, rows: 1 });
    put(&mut state, 0, 0, 'a', 1);
    put(&mut state, 0, 1, 'a', 1);
    put(&mut state, 0, 2, '世', 2);
    put(&mut state, 0, 3, ' ', 0);
    put(&mut state, 0, 4, 'a', 1);

    let clipped = state.clip_row(0, 4);
    assert!(!clipped.right_pad());
    assert_eq!(clipped.cells().len(), 4);
    assert_eq!(clipped.cells()[2].ch(), '世');
    assert_eq!(clipped.cells()[2].width(), 2);
    assert_eq!(clipped.cells()[3].width(), 0);
}

#[test]
fn clip_row_with_zero_inner_width_returns_no_cells() {
    let state = TerminalState::new(PtySize { cols: 5, rows: 1 });
    let clipped = state.clip_row(0, 0);
    assert!(!clipped.right_pad());
    assert!(clipped.cells().is_empty());
}

#[test]
fn clip_row_clamps_an_inner_width_past_the_row_length() {
    let mut state = TerminalState::new(PtySize { cols: 4, rows: 1 });
    for col in 0..4 {
        put(&mut state, 0, col, 'a', 1);
    }
    let clipped = state.clip_row(0, 10);
    assert!(!clipped.right_pad());
    assert_eq!(clipped.cells().len(), 4);
}

#[test]
fn clip_row_on_an_out_of_range_row_returns_no_cells() {
    let state = TerminalState::new(PtySize { cols: 4, rows: 2 });
    let clipped = state.clip_row(5, 4);
    assert!(!clipped.right_pad());
    assert!(clipped.cells().is_empty());
}

#[test]
fn clip_row_in_a_single_column_pane_pads_a_wide_glyph() {
    // A 1-column pane cannot hold a wide glyph's two halves; `print` stores the
    // base (width 2) in that lone column. Clipping must still pad, never show
    // the half.
    let mut state = TerminalState::new(PtySize { cols: 1, rows: 1 });
    put(&mut state, 0, 0, '世', 2);
    let clipped = state.clip_row(0, 1);
    assert!(clipped.right_pad());
    assert!(clipped.cells().is_empty());
}

#[test]
fn new_starts_with_an_empty_scrollback() {
    let state = TerminalState::new(PtySize { cols: 5, rows: 3 });
    assert!(state.scrollback().is_empty());
    assert_eq!(state.scrollback().len(), 0);
    assert_eq!(state.scrollback().dropped_lines(), 0);
    assert_eq!(state.scrollback().dropped_bytes(), 0);
}

/// A row of `s`, one default-styled cell per char — a scrollback line fixture.
fn line(s: &str) -> Vec<Cell> {
    s.chars()
        .map(|ch| Cell::new(ch, 1, Style::default()))
        .collect()
}

/// Read `row` of `grid` as a string; blank cells read as spaces.
fn grid_row(grid: &Grid, row: u16) -> String {
    let (_, cols) = grid.dimensions();
    (0..cols)
        .map(|c| grid.cell(row, c).map(Cell::ch).unwrap_or(' '))
        .collect()
}

/// A 3-wide, 2-row primary screen with live rows `L0`/`L1` and three retained
/// history rows `h0`/`h1`/`h2` (oldest first).
fn state_with_history() -> TerminalState {
    let mut state = TerminalState::new(PtySize { cols: 3, rows: 2 });
    for (col, ch) in "L0.".chars().enumerate() {
        *state.active_grid_mut().cell_mut(0, col as u16).unwrap() =
            Cell::new(ch, 1, Style::default());
    }
    for (col, ch) in "L1.".chars().enumerate() {
        *state.active_grid_mut().cell_mut(1, col as u16).unwrap() =
            Cell::new(ch, 1, Style::default());
    }
    state.scrollback.push_line(line("h0."), RowEnd::Hard);
    state.scrollback.push_line(line("h1."), RowEnd::Hard);
    state.scrollback.push_line(line("h2."), RowEnd::Hard);
    state
}

#[test]
fn scrolled_view_at_offset_zero_shares_the_live_buffer() {
    let state = state_with_history();
    // Offset 0 follows live: the same Arc (no compose, no copy) and effective 0.
    let (grid, effective) = state.scrolled_view(0);
    assert!(Arc::ptr_eq(&grid, &state.active_grid_arc()));
    assert_eq!(effective, 0);
}

#[test]
fn scrolled_view_composes_history_above_the_live_screen() {
    let state = state_with_history();
    // Offset 1: newest history row on top, top live row below.
    let (grid, effective) = state.scrolled_view(1);
    assert_eq!(grid.dimensions(), (2, 3));
    assert_eq!(grid_row(&grid, 0), "h2.");
    assert_eq!(grid_row(&grid, 1), "L0.");
    assert_eq!(effective, 1);
}

#[test]
fn scrolled_view_at_the_screen_height_shows_only_history() {
    let state = state_with_history();
    // Offset 2 == the 2-row screen height: both rows come from history.
    let (grid, effective) = state.scrolled_view(2);
    assert_eq!(grid_row(&grid, 0), "h1.");
    assert_eq!(grid_row(&grid, 1), "h2.");
    assert_eq!(effective, 2);
}

#[test]
fn scrolled_view_clamps_an_over_scroll_to_the_oldest_line() {
    let state = state_with_history();
    // Three history rows, screen height 2: offset 3 shows the oldest window,
    // and any larger offset clamps — grid and effective offset both — to that
    // same window rather than reading past.
    let (grid, effective) = state.scrolled_view(3);
    assert_eq!(grid_row(&grid, 0), "h0.");
    assert_eq!(grid_row(&grid, 1), "h1.");
    assert_eq!(effective, 3);

    let (over, over_effective) = state.scrolled_view(99);
    assert_eq!(grid_row(&over, 0), "h0.");
    assert_eq!(grid_row(&over, 1), "h1.");
    assert_eq!(over_effective, 3); // clamped to the retained count
}

#[test]
fn scrolled_view_on_the_alternate_screen_reports_a_live_zero_offset() {
    let mut state = state_with_history();
    state.active = Screen::Alternate; // full-screen apps keep no scrollback
    let (grid, effective) = state.scrolled_view(5);
    // The alternate screen always shows live: the live Arc and a zero effective
    // offset, so the indicator and cursor never treat it as scrolled.
    assert!(Arc::ptr_eq(&grid, &state.active_grid_arc()));
    assert_eq!(effective, 0);
}

#[test]
fn scrolled_view_with_empty_history_follows_live() {
    let state = TerminalState::new(PtySize { cols: 3, rows: 2 });
    let (grid, effective) = state.scrolled_view(5);
    assert!(Arc::ptr_eq(&grid, &state.active_grid_arc()));
    assert_eq!(effective, 0);
}

#[test]
fn scrolled_view_pads_narrow_history_rows_with_the_live_background() {
    // A history row captured 2 wide on a now-3-wide screen; the app has set a
    // background pen (SGR 48), so the padding carries it, not the default.
    let mut state = TerminalState::new(PtySize { cols: 3, rows: 2 });
    state.primary_render.style.set_bg(Color::Indexed(4));
    state.scrollback.push_line(line("ab"), RowEnd::Hard);

    let (grid, _) = state.scrolled_view(1);
    let padded = grid.cell(0, 2).unwrap();
    let mut expected = Style::default();
    expected.set_bg(Color::Indexed(4)); // bg-only fill: fg and attrs stay default
    assert_eq!(padded.ch(), ' ');
    assert_eq!(padded.style(), expected);
}
