//! Tests for drawing the floating pane tray: its border and header over the
//! floating panes, one ring per slot with the tile's number and title, a
//! title cut with `…` where it does not fit, the lit, gray, selected and empty
//! styles, the title of the tile under the pointer in the header, the tabline
//! button in reverse video while the tray is open, and no cursor under the
//! tray.

use super::*;

use ratatui::layout::{Position, Rect as RatatuiRect};
use ratatui::style::{Color, Modifier};

use koshi_terminal::grid::state::Grid;
use koshi_terminal::style::Style as TerminalStyle;
use koshi_test_support::fixtures::build_cell_rect;

use crate::floating_pane_tray::tests::{
    build_open_tray_viewer_chrome, build_tray_test_render_snapshot, show_tray_test_floating_pane,
    EIGHTY_BY_TWENTY_FOUR_SIZE,
};
use crate::images::ImageRenderMode;
use crate::render::style::compute_mode_style;
use crate::render::tests::{build_viewport_area, format_rendered_cells};
use crate::render::{build_frame_geometry, get_cursor_position, render_frame};
use crate::snapshot::{CommittedRegions, GridView, KeymapHints, ViewerChrome};

/// A session with floating panes `htop`, shown at `(16, 4)` `48x13`, and
/// `logs`, minimized, on an `80x24` viewport.
fn build_htop_and_logs_render_snapshot() -> RenderSnapshot {
    let mut render_snapshot =
        build_tray_test_render_snapshot(EIGHTY_BY_TWENTY_FOUR_SIZE, &["htop", "logs"]);
    show_tray_test_floating_pane(&mut render_snapshot, 0, build_cell_rect(16, 4, 48, 13));
    render_snapshot
}

/// Paint the rows `drawn_rows` of `render_snapshot` with `viewer_chrome` over
/// the compiled-in region solve of its viewport into `render_buffer`, in the
/// default theme, with placeholder images.
fn render_tray_frame_rows(
    render_snapshot: &RenderSnapshot,
    viewer_chrome: ViewerChrome,
    drawn_rows: DrawnRows<'_>,
    render_buffer: &mut Buffer,
) {
    render_frame(
        &build_frame_geometry(
            render_snapshot,
            &CommittedRegions::build_core(render_snapshot.client_snapshot.viewport_size, 0),
            viewer_chrome,
            build_viewport_area(render_snapshot),
        ),
        &Theme::default(),
        &KeymapHints::default(),
        None,
        ImageRenderMode::Placeholder,
        &[],
        None,
        None,
        drawn_rows,
        render_buffer,
    );
}

/// Paint every row of `render_snapshot` with `viewer_chrome` into a fresh
/// buffer of its viewport's size ([`render_tray_frame_rows`]).
fn render_tray_frame(render_snapshot: &RenderSnapshot, viewer_chrome: ViewerChrome) -> Buffer {
    let mut render_buffer = Buffer::empty(build_viewport_area(render_snapshot));
    render_tray_frame_rows(
        render_snapshot,
        viewer_chrome,
        DrawnRows::Every,
        &mut render_buffer,
    );
    render_buffer
}

/// The foreground and the modifiers of the cell at `(column, row)` in
/// `render_buffer`.
fn get_cell_color_and_modifier(render_buffer: &Buffer, column: u16, row: u16) -> (Color, Modifier) {
    let cell = &render_buffer[(column, row)];
    (cell.fg, cell.modifier)
}

/// The foreground and the modifiers `style` sets.
fn get_style_color_and_modifier(style: Style) -> (Color, Modifier) {
    (
        style.fg.expect("the style sets a foreground"),
        style.add_modifier,
    )
}

#[test]
fn the_open_tray_draws_its_header_its_slots_and_each_tiles_number_and_title() {
    let render_buffer = render_tray_frame(
        &build_htop_and_logs_render_snapshot(),
        build_open_tray_viewer_chrome(0),
    );

    let tray_rows: Vec<String> = (1..9)
        .map(|row_index| format_rendered_cells(&render_buffer, row_index, 5..74))
        .collect();
    assert_eq!(
        tray_rows,
        [
            "┌─ htop ───────────────────────────────────────────── minimize all ─┐",
            "│ ┌ 1 ─────┐ ┌ 2 ─────┐ ┌────────┐ ┌────────┐ ┌────────┐ ┌────────┐ │",
            "│ │ htop   │ │ logs   │ │        │ │        │ │        │ │        │ │",
            "│ └────────┘ └────────┘ └────────┘ └────────┘ └────────┘ └────────┘ │",
            "│ ┌────────┐ ┌────────┐ ┌────────┐ ┌────────┐ ┌────────┐ ┌────────┐ │",
            "│ │        │ │        │ │        │ │        │ │        │ │        │ │",
            "│ └────────┘ └────────┘ └────────┘ └────────┘ └────────┘ └────────┘ │",
            "└───────────────────────────────────────────────────────────────────┘",
        ]
    );
}

#[test]
fn a_title_too_long_for_its_cells_ends_in_an_ellipsis_and_every_tile_keeps_its_number() {
    let mut render_snapshot = build_tray_test_render_snapshot(
        EIGHTY_BY_TWENTY_FOUR_SIZE,
        &[
            "abcdefghijklmnopqrstuvwxyzabcdefghijklmnopqrstuvwxyz0123456789",
            "nginx",
            "a日本語テ",
            "postgres",
            "logserv",
            "untitled",
            "7",
            "8",
            "9",
            "10",
            "11",
            "12",
        ],
    );
    let FloatingPane::Minimized { pane_title, .. } =
        &mut render_snapshot.session_snapshot.floating_panes[5]
    else {
        panic!("every tray test pane starts minimized");
    };
    *pane_title = None;

    let render_buffer = render_tray_frame(&render_snapshot, build_open_tray_viewer_chrome(0));

    let tray_rows: Vec<String> = (1..9)
        .map(|row_index| format_rendered_cells(&render_buffer, row_index, 5..74))
        .collect();
    assert_eq!(
        tray_rows,
        [
            "┌─ abcdefghijklmnopqrstuvwxyzabcdefghijklmnopqrstuvw…─ restore all ─┐",
            "│ ┌ 1 ─────┐ ┌ 2 ─────┐ ┌ 3 ─────┐ ┌ 4 ─────┐ ┌ 5 ─────┐ ┌ 6 ─────┐ │",
            "│ │ abcdef…│ │ nginx  │ │ a日 本 … │ │ postgr…│ │ logserv│ │        │ │",
            "│ └────────┘ └────────┘ └────────┘ └────────┘ └────────┘ └────────┘ │",
            "│ ┌ 7 ─────┐ ┌ 8 ─────┐ ┌ 9 ─────┐ ┌ 10 ────┐ ┌ 11 ────┐ ┌ 12 ────┐ │",
            "│ │ 7      │ │ 8      │ │ 9      │ │ 10     │ │ 11     │ │ 12     │ │",
            "│ └────────┘ └────────┘ └────────┘ └────────┘ └────────┘ └────────┘ │",
            "└───────────────────────────────────────────────────────────────────┘",
        ]
    );
}

#[test]
fn a_closed_tray_draws_nothing_over_the_floating_pane() {
    let render_snapshot = build_htop_and_logs_render_snapshot();
    let open_render_buffer = render_tray_frame(&render_snapshot, build_open_tray_viewer_chrome(0));
    let closed_render_buffer = render_tray_frame(&render_snapshot, ViewerChrome::default());

    // The `htop` box takes screen rows 5 to 17: its titlebar on row 6 and
    // its first content row on row 8, the tray's bottom border.
    assert_eq!(
        format_rendered_cells(&open_render_buffer, 8, 17..21),
        "────"
    );
    assert_eq!(
        format_rendered_cells(&closed_render_buffer, 8, 17..21),
        "    "
    );
    assert_eq!(
        format_rendered_cells(&closed_render_buffer, 6, 16..24),
        "│ ⣿⣿  ht"
    );
}

#[test]
fn a_lit_tile_a_gray_tile_the_selected_ring_and_an_empty_slot_take_their_styles() {
    let theme = Theme::default();
    let render_buffer = render_tray_frame(
        &build_htop_and_logs_render_snapshot(),
        build_open_tray_viewer_chrome(0),
    );

    // Tile 0 shows `htop`, which the viewer shows, and is selected.
    assert_eq!(
        get_cell_color_and_modifier(&render_buffer, 9, 3),
        get_style_color_and_modifier(compute_placement_hover_border_style(&theme))
    );
    assert_eq!(
        get_cell_color_and_modifier(&render_buffer, 7, 2),
        get_style_color_and_modifier(compute_hover_border_style(&theme))
    );
    // Tile 1 shows `logs`, which the viewer minimized.
    assert_eq!(
        get_cell_color_and_modifier(&render_buffer, 20, 3),
        get_style_color_and_modifier(compute_unfocused_border_style(&theme))
    );
    assert_eq!(
        get_cell_color_and_modifier(&render_buffer, 18, 2),
        get_style_color_and_modifier(compute_unfocused_border_style(&theme))
    );
    // Slot 2 shows no pane.
    assert_eq!(
        get_cell_color_and_modifier(&render_buffer, 29, 2),
        get_style_color_and_modifier(compute_unfocused_border_style(&theme))
    );
    // The tray's ring and header.
    assert_eq!(
        get_cell_color_and_modifier(&render_buffer, 5, 1),
        get_style_color_and_modifier(compute_focused_border_style(&theme))
    );
    assert_eq!(
        get_cell_color_and_modifier(&render_buffer, 60, 1),
        get_style_color_and_modifier(compute_focused_border_style(&theme))
    );
}

#[test]
fn the_selected_ring_moves_with_the_selected_tile() {
    let theme = Theme::default();
    let render_buffer = render_tray_frame(
        &build_htop_and_logs_render_snapshot(),
        build_open_tray_viewer_chrome(1),
    );

    assert_eq!(
        get_cell_color_and_modifier(&render_buffer, 7, 2),
        get_style_color_and_modifier(compute_placement_hover_border_style(&theme))
    );
    assert_eq!(
        get_cell_color_and_modifier(&render_buffer, 18, 2),
        get_style_color_and_modifier(compute_hover_border_style(&theme))
    );
    assert_eq!(
        format_rendered_cells(&render_buffer, 1, 5..15),
        "┌─ logs ──"
    );
}

#[test]
fn the_header_names_the_tile_under_the_pointer_before_the_selected_tile() {
    let mut viewer_chrome = build_open_tray_viewer_chrome(0);
    viewer_chrome.floating_pane_tray = Some(FloatingPaneTray {
        selected_tile_index: 0,
        hovered_tile_index: Some(1),
    });

    let render_buffer = render_tray_frame(&build_htop_and_logs_render_snapshot(), viewer_chrome);

    assert_eq!(
        format_rendered_cells(&render_buffer, 1, 5..15),
        "┌─ logs ──"
    );
}

#[test]
fn every_pane_minimized_reads_restore_all_in_the_header() {
    let render_snapshot =
        build_tray_test_render_snapshot(EIGHTY_BY_TWENTY_FOUR_SIZE, &["htop", "logs"]);

    let render_buffer = render_tray_frame(&render_snapshot, build_open_tray_viewer_chrome(0));

    assert_eq!(
        format_rendered_cells(&render_buffer, 1, 57..74),
        "── restore all ─┐"
    );
}

#[test]
fn the_tabline_button_shows_while_the_session_holds_a_floating_pane_and_reverses_while_open() {
    let theme = Theme::default();
    let render_snapshot = build_htop_and_logs_render_snapshot();
    let closed_render_buffer = render_tray_frame(&render_snapshot, ViewerChrome::default());
    let open_render_buffer = render_tray_frame(&render_snapshot, build_open_tray_viewer_chrome(0));

    assert_eq!(
        format_rendered_cells(&closed_render_buffer, 0, 71..80),
        " ⧉  BASE "
    );
    assert_eq!(
        format_rendered_cells(&open_render_buffer, 0, 71..80),
        " ⧉  TRAY "
    );
    assert_eq!(
        get_cell_color_and_modifier(&closed_render_buffer, 72, 0),
        get_style_color_and_modifier(compute_mode_style(&theme))
    );
    assert_eq!(
        get_cell_color_and_modifier(&open_render_buffer, 72, 0),
        get_style_color_and_modifier(compute_mode_style(&theme).add_modifier(Modifier::REVERSED))
    );
}

#[test]
fn a_session_with_no_floating_pane_draws_the_tabline_it_drew_before_the_button() {
    let render_snapshot = build_tray_test_render_snapshot(EIGHTY_BY_TWENTY_FOUR_SIZE, &[]);

    let render_buffer = render_tray_frame(&render_snapshot, ViewerChrome::default());

    assert_eq!(
        format_rendered_cells(&render_buffer, 0, 68..80),
        "       BASE "
    );
}

#[test]
fn the_cursor_under_the_open_tray_is_hidden_and_shows_once_the_tray_closes() {
    let mut render_snapshot =
        build_tray_test_render_snapshot(EIGHTY_BY_TWENTY_FOUR_SIZE, &["htop"]);
    // The tiled pane's content starts at screen (1, 2): its cursor at row 0,
    // column 20 sits at (21, 2), inside the tray.
    let tiled_pane_snapshot = &mut render_snapshot.pane_snapshots[0];
    tiled_pane_snapshot.terminal_grid_view = Some(GridView::from_grid(
        &Grid::build_blank(20, 78, TerminalStyle::default()),
        0,
    ));
    tiled_pane_snapshot.cursor_snapshot.column_index = 20;
    tiled_pane_snapshot.cursor_snapshot.is_visible = true;
    let committed_regions = CommittedRegions::build_core(EIGHTY_BY_TWENTY_FOUR_SIZE, 0);
    let viewport_area = RatatuiRect::new(0, 0, 80, 24);

    assert_eq!(
        get_cursor_position(&build_frame_geometry(
            &render_snapshot,
            &committed_regions,
            build_open_tray_viewer_chrome(0),
            viewport_area
        )),
        None
    );
    assert_eq!(
        get_cursor_position(&build_frame_geometry(
            &render_snapshot,
            &committed_regions,
            ViewerChrome::default(),
            viewport_area
        )),
        Some(Position::new(21, 2))
    );
}

#[test]
fn each_listed_row_draws_the_open_tray_as_a_frame_of_every_row_and_other_rows_keep_their_cells() {
    let render_snapshot = build_htop_and_logs_render_snapshot();
    let viewer_chrome = build_open_tray_viewer_chrome(0);
    let every_row_buffer = render_tray_frame(&render_snapshot, viewer_chrome);
    let viewport_area = build_viewport_area(&render_snapshot);
    let row_width = usize::from(viewport_area.width);
    let list_row_cells = |render_buffer: &Buffer, row_index: u16| {
        let row_start = usize::from(row_index) * row_width;
        render_buffer.content[row_start..row_start + row_width].to_vec()
    };

    // The tray takes rows 1 to 8: row 1 holds its header, rows 3 and 6 the
    // tile titles, and row 12 lies below it.
    for listed_rows in [vec![1], vec![3], vec![2, 5, 8], vec![0, 6, 12], vec![12]] {
        let mut listed_row_buffer = Buffer::empty(viewport_area);
        for stale_cell in &mut listed_row_buffer.content {
            stale_cell.set_symbol("Z");
        }
        let stale_row_cells = list_row_cells(&listed_row_buffer, 0);

        render_tray_frame_rows(
            &render_snapshot,
            viewer_chrome,
            DrawnRows::Listed(&listed_rows),
            &mut listed_row_buffer,
        );

        for row_index in 0..viewport_area.height {
            let expected_row_cells = if listed_rows.contains(&row_index) {
                list_row_cells(&every_row_buffer, row_index)
            } else {
                stale_row_cells.clone()
            };
            assert_eq!(
                list_row_cells(&listed_row_buffer, row_index),
                expected_row_cells,
                "listed rows {listed_rows:?}, row {row_index}"
            );
        }
    }
}
