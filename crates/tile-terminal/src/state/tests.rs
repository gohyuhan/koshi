//! Unit tests for per-pane terminal state.

use super::*;
use crate::grid::state::Grid;
use crate::style::{Color, Style};

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
    assert_eq!(
        state.cursor,
        Cursor {
            row: 0,
            col: 0,
            is_visible: true,
            saved: None,
            pending_wrap: false,
        }
    );
    assert_eq!(state.style, Style::default());
    assert_eq!(state.modes, TerminalModes {});
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
fn resize_fills_the_new_grids_with_the_current_background() {
    let mut state = TerminalState::new(PtySize { cols: 80, rows: 24 });
    state.style.set_bg(Color::Indexed(4)); // blue pen active at resize time
    state.resize(PtySize { cols: 4, rows: 2 });

    let mut blue_fill = Style::default();
    blue_fill.set_bg(Color::Indexed(4)); // bg-only: fg + attrs stay default
    assert_eq!(state.primary, Grid::blank(2, 4, blue_fill));
    assert_eq!(state.alternate, Grid::blank(2, 4, blue_fill));
}

#[test]
fn resize_clamps_out_of_bounds_cursor_to_last_cell() {
    let mut state = TerminalState::new(PtySize { cols: 80, rows: 24 });
    state.cursor.row = 23;
    state.cursor.col = 79;
    state.resize(PtySize { cols: 10, rows: 5 });
    assert_eq!(state.cursor.row, 4);
    assert_eq!(state.cursor.col, 9);
}

#[test]
fn resize_leaves_in_bounds_cursor_untouched() {
    let mut state = TerminalState::new(PtySize { cols: 80, rows: 24 });
    state.cursor.row = 2;
    state.cursor.col = 3;
    state.resize(PtySize { cols: 10, rows: 5 });
    assert_eq!(state.cursor.row, 2);
    assert_eq!(state.cursor.col, 3);
}

#[test]
fn resize_clears_a_pending_wrap_latched_to_the_old_edge() {
    let mut state = TerminalState::new(PtySize { cols: 80, rows: 24 });
    state.cursor.pending_wrap = true;
    state.resize(PtySize { cols: 10, rows: 5 });
    assert!(!state.cursor.pending_wrap);
}
