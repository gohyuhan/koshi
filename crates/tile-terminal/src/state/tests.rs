//! Unit tests for per-pane terminal state.

use super::*;
use crate::grid::state::{Cell, Grid};
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
    assert_eq!(state.primary, Grid::blank(3, 5, Style::default()));
    assert_eq!(state.alternate, Grid::blank(3, 5, Style::default()));
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
    assert!(std::ptr::eq(state.active_grid(), &state.primary));
    state.active = Screen::Alternate;
    assert!(std::ptr::eq(state.active_grid(), &state.alternate));
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
    assert_eq!(state.primary, Grid::blank(5, 10, Style::default()));
    assert_eq!(state.alternate, Grid::blank(5, 10, Style::default()));
}

#[test]
fn resize_fills_each_grid_with_its_own_screen_background() {
    // Each screen's grid is filled with that screen's own render background color,
    // not the other screen's background.
    let mut state = TerminalState::new(PtySize { cols: 80, rows: 24 });
    state.primary_render.style.set_bg(Color::Indexed(4)); // primary: blue
    state.alternate_render.style.set_bg(Color::Indexed(1)); // alternate: red
    state.resize(PtySize { cols: 4, rows: 2 });

    let mut blue_fill = Style::default();
    blue_fill.set_bg(Color::Indexed(4)); // bg-only: fg + attrs stay default
    let mut red_fill = Style::default();
    red_fill.set_bg(Color::Indexed(1));
    assert_eq!(state.primary, Grid::blank(2, 4, blue_fill)); // primary keeps its own blue
    assert_eq!(state.alternate, Grid::blank(2, 4, red_fill)); // alternate keeps its own red
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
